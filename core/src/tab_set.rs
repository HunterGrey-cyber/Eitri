//! One window's session tabs: one conversation, and so one `AgentBackend`,
//! per tab, every one of them running. GTK-free: `eitri_panel::agent_panel` spawns the connect and
//! handoff workers and hands this set their receivers; the panel's tick calls [`TabSet::pump`], which
//! drains EVERY tab -- `take_ui_delivery` is where the permission policy answers what needs no
//! human, so a tab that was not pumped would stall on its first `Read`.
//!
//! **Lock order, inherited:** on the sidecar path `projection()` holds the ingestion mutex and
//! `provider_session_id()` takes it again. Never call the second while holding the first's guard
//! (the 2026-09-15 GTK freeze). Every method below reads them in separate statements.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Weak};

use agent::{AgentDomainEvent, RevisedDelivery};

pub use crate::agent_backend::AgentId;
use crate::agent_backend::{AgentBackend, BackendError, BackendKind};
use crate::agent_bridge::{
    serialize_events_with_notes_for_js, serialize_handoff_for_js, serialize_snapshot_with_notes_for_js,
    serialize_tabs_for_js, CallNotes, SessionModeChoice, SnapshotView, TabRef, TabStateWire, TabView, TurnTime,
};
use crate::attention::{Attention, AttentionTracker};
use crate::saved_tabs::{SavedExternalTab, SavedTab, SavedTabs};
use crate::tab_restore::{PlannedTab, RestorePlan, RestoreRun, Skipped};
use crate::tabs::{self, TabFacts, TabId};

/// The first turn of a session started by it, composed when Enter was pressed.
pub struct FirstTurn {
    /// With the editor context composed in: what goes to the model.
    pub wire: String,
    /// As typed: what the panel shows and what titles the session.
    pub typed: String,
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
/// Build the command before moving the session to the worker, while its state is readable.
/// Dispatch it only after `closed_rx` reports, so the session is closed and flushed before
/// the user can start the displayed command. Closing runs off the GTK main loop because
/// the sidecar's `close_session` RPC can block for up to 10 seconds and freeze the editor.
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

/// One message typed while a turn ran. `wire` uses the editor context at the moment it was
/// queued; `text` is what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        }
    }

    pub fn is_held(&self) -> bool {
        self.state == QueuedState::Held
    }
}

/// Where a typed message is: waiting for the turn to end, or sent into the running turn and not yet
/// read by the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueuedState {
    /// Goes out with the next turn (`TabSet::flush_queue`), or into this one when the agent can take it.
    Held,
    /// Sent into the running turn under `client_id` (`AgentBackend::steer`); it leaves the queue when
    /// the agent reads it, and is held again if the agent refuses it or the turn ends first.
    Steered { client_id: String, sent_at_ms: u64 },
}

/// What a flush sent, and how the backend answered.
pub struct Flush {
    pub typed: String,
    pub outcome: Result<Vec<agent::AgentDomainEvent>, BackendError>,
}

pub enum SendNow {
    /// A turn was running: it was asked to stop; the queue goes out when it ends.
    Interrupting(Result<Vec<agent::AgentDomainEvent>, BackendError>),
    /// Idle: flushed at once (`None` when there was nothing to send).
    Flushed(Option<Flush>),
}

/// Not boxed: a window holds a handful of tabs, `Live` is the state a tab spends its life in, and
/// the plan's interface matches on `Live(AgentBackend)` directly.
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
    pub id: TabId,
    pub number: u16,
    /// The rename (`prefix ,`), already normalized.
    pub name: Option<String>,
    /// The first prompt's title, or the resumed record's display title.
    pub title: Option<String>,
    /// `title` came from the record a resume or restore was started from, before the CLI's own title
    /// for the session may have been read: a later read replaces it ([`TabSet::retitle_from`]).
    pub title_from_record: bool,
    /// The tab's permission mode survives panel reloads. Keep it private so entering bypass can
    /// only happen through `cycle_mode`/`confirm_bypass`, which require confirmation.
    /// `mode()` provides read-only access; this module's tests may write the field directly.
    mode: SessionModeChoice,
    /// The agent this tab runs, chosen when it was opened (or by the session a restore brings back
    /// into it) and kept by `r`. Private so that only the set decides it; [`Tab::agent`] reads it.
    agent: AgentId,
    pub backend: TabBackend,
    /// Which of the project's own configuration tiers the session of this tab was given; set when
    /// the session is spawned, so a later change of the window's answer leaves running sessions as
    /// they were.
    pub project_tiers: Option<agent::setting_sources::ProjectTrust>,
    pub attention: AttentionTracker,
    /// The WebView's copy is behind: this tab took events while it was not active.
    pub stale: bool,
    pub turn_trace: Option<crate::turn_trace::TurnTrace>,
    pub reported_start_failure: bool,
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
    /// Held per tab here so a panel reload cannot lose the last terminal handoff.
    pub last_handoff: Option<agent::handoff::ClaudeResumeCommand>,
    /// Messages typed while a turn ran, oldest first. Flushed as one turn when the
    /// turn ends, never while a card waits.
    pub queue: Vec<Queued>,
    /// Why the last flush was refused. Cleared by a flush that goes out, a take-back or
    /// a reset.
    pub queue_error: Option<String>,
    /// Why messages typed while the turn ran are held rather than in the turn (the agent cannot take
    /// them, refused one, or the turn ended before it read one). Shown once under the queue; cleared when
    /// nothing is held any more.
    pub queue_note: Option<String>,
    /// The panel's composer text, mirrored here so a switch or a reload cannot lose it.
    pub draft: String,
    /// Permission id -> the rule the card offers: only for a card a rule would answer.
    pub rule_offers: std::collections::BTreeMap<String, agent::PrefixRule>,
    /// The scratch edit of the draft in nvim, when one is open (set by the shell's round trip).
    pub editing_draft: Option<u64>,
    /// The comments and reverts collected in this tab's turn review. Held here, not in the WebView,
    /// so a panel reload keeps it; it goes with the tab and is not saved.
    pub review_draft: crate::turn_review::ReviewDraft,
    /// Whether a turn is running in what the pump has DELIVERED (a `TurnStarted` without its
    /// `TurnCompleted`), so it reports a turn's end exactly once. Read off the event
    /// stream rather than re-read from `projection()` after the delivery: on the sidecar path the
    /// ingestion thread folds into the projection concurrently, so a re-read can already include a
    /// completion the delivery did not carry, and the end was then never reported.
    was_running: bool,
    /// What the rows and cards say beyond the projection, kept for every later snapshot: the saved
    /// rule that answered a call (`note_rule_answers`), and the `Write` cards whose
    /// file did not exist when they were raised (`note_new_files`).
    notes: CallNotes,
    /// Tool-use id -> the rule and `(tool_name, input)` whose gate
    /// `AgentBackend::answer_what_needs_no_human` answered. Kept until the call completes, then moved
    /// to `notes.allowed_by_rule`, like `auto_edit_candidates`.
    /// Only `AnsweredForYou::by_rule` fills this map. A call merely starting is not evidence that a
    /// rule answered it: CLI validation may fail first, or bypass may answer the gate instead.
    /// A later card for the same call removes the candidate, by id or by matching `(tool_name, input)`
    /// when no id is supplied. The provider may send `toolUseId: ''`, which `translate.rs` maps to
    /// `None`; matching only by id would wrongly label a later human approval as "allowed by rule".
    /// A `Resync` converts candidates whose calls finished among dropped events into notes. It drops
    /// candidates still awaiting a human in the projection, since overflow can lose the
    /// `PermissionRequested` that would have removed them. Cleared on fresh backend installation
    /// (`collect_starts`).
    rule_candidates: std::collections::BTreeMap<String, RuleCandidate>,
    /// Tool-use id -> the `(tool_name, input)` seen by the acceptEdits fast path when it answered
    /// a `Write`/`Edit`/`NotebookEdit` gate. Completion turns it into `notes.allowed_by_auto` through
    /// `note_auto_edit_answers`. Fill only from `AgentBackend::take_revised_ui_delivery` answers
    /// marked by `AnsweredForYou::by_the_fast_path`. A call can finish without a card because CLI
    /// validation failed before any gate, so completion alone is insufficient. A later card drops
    /// the candidate by id, or by matching `(tool_name, input)` if the card has no id, just as for
    /// `rule_candidates`. `Resync` turns candidates completed among dropped events into notes.
    /// Clear with `prompt_note_candidates`.
    auto_edit_candidates: std::collections::BTreeMap<String, CandidateCall>,
    /// Tool-use ids already in `notes.auto_creates_file` that no events payload has carried yet
    ///. The note is made when the gate is answered, and a gate
    /// usually arrives in a batch of its own that the answer leaves empty (`RevisedDelivery::
    /// Nothing`, no payload), so it waits here for the next one; a `Resync`'s snapshot carries every
    /// note and empties it. Cleared with `auto_edit_candidates`.
    creates_file_unsent: BTreeSet<String>,
    /// The pending permission ids `rule_offers` was last computed for, sorted, so the classifier
    /// (which canonicalizes `Bash` arguments) runs once per change rather than every tick.
    rule_offers_seen: Vec<String>,
    /// The running turn's id and when (wall clock, ms since the epoch) the first tick saw it
    /// running. The panel holds only the active tab's state, so without this a switch back to a tab
    /// mid-turn restarted its elapsed clock at `0s+` (the phase-3 GUI pass, 2026-09-25); the
    /// snapshot carries it as `turnStartedAtMs`. Stamped from the projection, not from a
    /// `TurnStarted` event, because a legacy `TurnStarted` is returned by `send_turn` and never
    /// pumped -- so it is at most one 33 ms tick late, on both backends.
    turn_clock: Option<(String, u64)>,
    /// How many of the newest entries in `notes.turn_times` no payload has carried yet. The active
    /// tab's events payload takes exactly those; a snapshot carries all of them and zeroes this. A
    /// background tab's count grows until a switch sends its snapshot.
    turn_times_unsent: usize,
    /// Permission ids this tab answered `allow` for, through `approve_pending` in bypass or
    /// `answer_what_needs_no_human` under the classifier, while awaiting `PermissionResolved`.
    /// Hide these from `SnapshotView::of` and the tray's resync/attention feed so answered requests
    /// never draw or count as cards. Only `tab_set` may insert, keeping the hiding rule within
    /// this module. This module's tests read the set directly.
    host_answered: BTreeSet<String>,
    /// The calls the user approved on a card in this tab, this turn, each with the tool and input the
    /// card showed: the CLI's own prompt for exactly such a call, which follows the
    /// gate's request for it, is answered `allow` without a second card, once -- as the real CLI asks
    /// once. Written only by `TabSet::answer_card` on an Approve that reached the provider; emptied
    /// when the turn ends (the CLI asks within the turn that made the call), on EVERY `Resync` (one
    /// can swallow a turn boundary -- Codex's finding), on `r` and with the backend. Private for the
    /// reason `host_answered` is.
    human_allowed: crate::agent_backend::HumanApprovals,
    /// Tool-use id -> the note for a call whose CLI prompt was answered without a card.
    /// On completion it becomes `notes.prompt_notes`, as a rule candidate does.
    prompt_note_candidates: std::collections::BTreeMap<String, String>,
    /// Permission ids the user answered on a card here (`TabSet::answer_card`, any response)
    /// that the projection still lists as pending until the provider's `PermissionResolved` arrives.
    /// `refresh_offers` never offers a rule for them again, and [`Tab::card_is_waiting`] returns false,
    /// so replaying a response after a tab switch or panel reload cannot save a rule. Removing an
    /// offer only once is insufficient: a later pump could recreate it when another id arrives.
    /// Pruned with `host_answered` once the id leaves the projection, and private for the same reason.
    user_answered: BTreeSet<String>,
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

/// Keeps the row note of each CLI prompt the host answered that names its call, for the call's row
/// once it completes. Takes the map, not the tab, so a caller holding the tab's backend borrowed can
/// still use it.
fn note_answered_prompts(
    candidates: &mut std::collections::BTreeMap<String, String>,
    prompts: Vec<crate::agent_backend::PromptAnsweredForYou>,
) {
    candidates.extend(
        prompts
            .into_iter()
            .map(|answered| (answered.tool_use_id, answered.note)),
    );
}

impl Tab {
    /// Every request this tab's host answered itself (a deferral or an `allow`), recorded where the
    /// answer was sent, so it holds one a resolution had already removed from the projection.
    #[cfg(any(test, feature = "test-support"))]
    pub fn gate_answers(&self) -> &[crate::agent_backend::GateAnswer] {
        &self.gate_log
    }

    fn new(id: TabId, number: u16, mode: SessionModeChoice, agent: AgentId) -> Self {
        Tab {
            id,
            number,
            name: None,
            title: None,
            title_from_record: false,
            mode,
            agent,
            backend: TabBackend::NotStarted,
            project_tiers: None,
            attention: AttentionTracker::default(),
            stale: false,
            turn_trace: None,
            reported_start_failure: false,
            failed_after_start: false,
            failed_resume: None,
            pending_handoff: None,
            last_handoff: None,
            queue: Vec::new(),
            queue_error: None,
            queue_note: None,
            draft: String::new(),
            rule_offers: std::collections::BTreeMap::new(),
            editing_draft: None,
            review_draft: crate::turn_review::ReviewDraft::default(),
            was_running: false,
            rule_offers_seen: Vec::new(),
            notes: CallNotes::default(),
            rule_candidates: std::collections::BTreeMap::new(),
            auto_edit_candidates: std::collections::BTreeMap::new(),
            creates_file_unsent: BTreeSet::new(),
            turn_clock: None,
            turn_times_unsent: 0,
            host_answered: BTreeSet::new(),
            human_allowed: crate::agent_backend::HumanApprovals::default(),
            prompt_note_candidates: std::collections::BTreeMap::new(),
            user_answered: BTreeSet::new(),
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

    /// Whether `permission_id` is still waiting for the user: a live backend's projection lists it
    /// as pending and `user_answered` does not contain it. The panel checks this before saving an
    /// "Always allow" rule.
    pub fn card_is_waiting(&self, permission_id: &str) -> bool {
        !self.user_answered.contains(permission_id)
            && self
                .live()
                .is_some_and(|backend| backend.projection().pending_permissions.contains_key(permission_id))
    }

    /// When this tab's running turn started, if one is running (see `turn_clock`).
    pub fn turn_started_at_ms(&self) -> Option<u64> {
        self.turn_clock.as_ref().map(|(_, at)| *at)
    }

    /// The tab's own permission mode. Read-only outside this module -- see the field's own doc.
    pub fn mode(&self) -> SessionModeChoice {
        self.mode
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
                TabBackend::Live(AgentBackend::External(external)) => external.conversation().resumes(),
                TabBackend::Starting(pending) => pending.resume.is_some(),
                TabBackend::AwaitingTrust(AwaitingTrust {
                    start: DeferredStart::Resume { .. },
                    ..
                }) => true,
                _ => false,
            },
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

    pub fn turn_running(&self) -> bool {
        self.live()
            .is_some_and(|backend| backend.projection().active_turn_id.is_some())
    }

    /// Applies what the provider reported about the messages this tab put into the running turn: one the
    /// agent read leaves the queue (its row is in the conversation now), one it did not is held again
    /// where it stood, with the reason. Read from the projection's record (`steer_outcome`), not from a
    /// batch of events, so a resync that dropped the events loses none of it. Whether the queue changed.
    fn settle_steered(&mut self) -> bool {
        if self.queue.iter().all(|q| q.is_held() && q.dropped_as.is_empty()) {
            return false;
        }
        let TabBackend::Live(backend) = &self.backend else {
            return false;
        };
        let projection = backend.projection();
        settle_against(&mut self.queue, &mut self.queue_note, &projection)
    }

    /// The tab's session is being taken away (a tripwire): what it reported is applied, and a message
    /// still in the turn is held again, since nothing will report on it any more.
    /// Whether the queue changed.
    fn hold_steered_on_failure(&mut self) -> bool {
        let settled = self.settle_steered();
        if self.queue.iter().all(Queued::is_held) {
            return settled;
        }
        for item in self.queue.iter_mut() {
            item.state = QueuedState::Held;
        }
        self.queue_note = Some(held_note("the session was closed before the agent read it"));
        true
    }

    /// Whether Enter while a turn runs puts the message into it: the live session's agent can take one now.
    /// `None` without a live session.
    pub fn steers(&self) -> Option<bool> {
        self.live().map(AgentBackend::steers)
    }

    /// The messages sent into the running turn that the agent has not been seen to read.
    pub fn steered_unconfirmed(&self) -> usize {
        self.queue.iter().filter(|q| !q.is_held()).count()
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
        tabs::label_name(self.name.as_deref(), self.title.as_deref(), id.as_deref())
    }

    fn view(&self, resumable: bool) -> TabView {
        let facts = self.facts();
        TabView {
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
            title: self.title.clone(),
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
    /// Tabs failed this tick because their CLI reported an ungated permission mode, with each
    /// backend removed for shutdown. The caller shuts them down off the GTK thread, as it does for
    /// closed tabs (`eitri_panel::agent_panel`'s `Retiring::backend`).
    pub tripped: Vec<Tripped>,
    /// Turns whose end snapshot landed this tick, for the status band (empty without a turn
    /// review installed).
    pub review_hints: Vec<crate::turn_review::ReviewHint>,
}

/// One tab the CLI-mode tripwire failed. See `PumpOutput::tripped`.
pub struct Tripped {
    pub tab: TabId,
    /// The same text the tab's `Failed` reason carries.
    pub reason: String,
    /// The session, still running until the caller shuts it down: its shutdown is what denies the
    /// requests left pending in it.
    pub backend: AgentBackend,
}

/// The line under the queue that says why its messages wait for the turn to end.
fn held_note(why: &str) -> String {
    format!("held until the turn ends: {why}")
}

/// [`Tab::settle_steered`] against a projection the caller already reads, so whatever else it decides
/// from the same read (whether a turn runs) is consistent with what the agent reported.
fn settle_against(
    queue: &mut Vec<Queued>,
    note: &mut Option<String>,
    projection: &agent::AgentSessionProjection,
) -> bool {
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
                dropped = Some(reason.clone());
                item.dropped_as.push(client_id.clone());
                item.state = QueuedState::Held;
                changed = true;
                true
            }
            None => true,
        }
    });
    if let Some(reason) = dropped {
        *note = Some(held_note(&reason));
    } else if changed && !queue.iter().any(Queued::is_held) {
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

/// Whether entering bypass changes one tab's mode or the window's remembered `default_mode`
/// for tabs not yet started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassScope {
    Tab(TabId),
    Default,
}

/// A pending "enter bypass" prompt, held in `TabSet::pending_bypass` until `y`/`n` answers it.
/// `serialize_confirm_bypass_for_js` sends only `lines`. Rust keeps `approve` so the panel
/// never counts cards or echoes an id back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BypassPlan {
    pub scope: BypassScope,
    /// Identifies the exact prompt being accepted, so a stale confirmation cannot accept a newer one.
    pub nonce: u64,
    /// The delivered cards this entry would approve if confirmed now, `seq`-ordered -- the same set
    /// `confirm_bypass` re-derives fresh (`waiting_cards`) and compares against before acting.
    pub approve: Vec<String>,
    /// Lines shown by `tabs::bypass_prompt`: the y/n prompt and any warning.
    pub lines: Vec<String>,
    /// Which `tabs::bypass_prompt` text `lines` contains, and thus what the user is agreeing to.
    /// A live-tab prompt says nothing about new sessions; an empty-tab prompt says they use bypass too.
    /// `confirm_bypass` checks this again and reprompts if it changed. A terminal handoff can turn a
    /// live tab into `NotStarted` under an open prompt; its `y` must not silently change the window
    /// default to bypass.
    pub prompt: tabs::PromptScope,
}

/// What a stop of the background says when the agent left nothing running.
pub const NOTHING_IN_THE_BACKGROUND: &str = "nothing is running in the background";

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
    /// Entered bypass, approving `approved` cards from the plan. `resolved` holds the local
    /// `AgentBackend::approve_pending` events: legacy `PermissionResolved`s that no pump carries.
    /// The caller must send them to the panel or approved cards stay drawn. Empty on the sidecar.
    Entered {
        approved: usize,
        resolved: Vec<AgentDomainEvent>,
    },
    /// The tab was already in bypass by the time this confirm arrived (a race, not an error):
    /// nothing to do.
    AlreadyBypass,
    /// A delivered card arrived after the plan was built and is absent from `approve`.
    /// Nothing changed; show the fresh plan's prompt instead.
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
    /// At most one pending "enter bypass" prompt. A fresh mode-key press supersedes it;
    /// a tab-switching call drops it.
    pending_bypass: Option<BypassPlan>,
    /// The next `BypassPlan::nonce`, starting at 1 so `0` is never a valid nonce to compare against.
    next_nonce: u64,
    /// Numbers the messages put into a running turn, for their client ids.
    next_steer: u64,
    /// Every card the tabs removed from this set had been handed, so the window's `arrived`
    /// (`attention`) never goes down when a tab closes: [`crate::attention::react`] reads only its
    /// growth.
    removed_arrived: u64,
    /// Project prefix rules shared by the window. Every tab's pump applies them.
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
    /// and never on the legacy backend, whose turn starts never pass through the pump. Only the
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
    /// a tripped or failed session), each with how many such shutdowns are outstanding. Their
    /// presence is kept as if a turn ran: an agent's subagent can still write during the shutdown's
    /// grace. [`TabSet::retire_done`] ends one.
    retiring: std::collections::BTreeMap<u64, usize>,
    /// What every live backend calls when it folded something, so the window need not poll for it
    /// ([`set_ui_wake`](Self::set_ui_wake)).
    ui_wake: Option<agent::UiWake>,
    /// One-shot test hook after `pump` drains a live tab's queue. Fold an event after the drain
    /// and before the next snapshot read in `active_state_payloads` or `Resync`, so the test
    /// reproduces the race without relying on timing.
    #[cfg(test)]
    after_drain: Option<BackendSeam>,
    /// Test-only: also deliver events already covered by a snapshot, so tests can demonstrate
    /// duplicate delivery and capture both versions for the reducer test.
    #[cfg(test)]
    redeliver_covered: bool,
    /// One-shot test hook just before the `Resync` snapshot read, after the early turn-clock read,
    /// the three `projection()` calls and `approve_pending`'s bypass sweep. This reaches the race
    /// that `after_drain`, which runs before those lock cycles, cannot.
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
            retiring: std::collections::BTreeMap::new(),
            ui_wake: None,
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
            if !tab.title_from_record {
                continue;
            }
            let Some(session) = tab.live().and_then(AgentBackend::provider_session_id) else {
                continue;
            };
            // Once: the tab takes the CLI's title as it is when first known (the old synchronous read at the
            // resume did no more), or keeps its own when that already is it, and follows no later retitle.
            if let Some(title) = title_of(&session) {
                tab.title_from_record = false;
                if tab.title.as_ref() != Some(&title) {
                    tab.title = Some(title);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Whether something here can become ready without telling anyone, so the window has to keep
    /// pumping on its tick: a connect or a terminal handoff running on a worker, a live backend that does
    /// not announce its events (the legacy one, or any before [`set_ui_wake`](Self::set_ui_wake)), or turn
    /// review waiting for its workers. When this is false, nothing changes here until a backend's wake, or
    /// a command from the window, says so.
    pub fn needs_polling(&self) -> bool {
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

    /// Starts reviewing turns. Refused on the legacy backend: its turn starts are returned by the
    /// send rather than delivered, so the review would never see a turn begin.
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

    /// Hands the presence the ids of the tabs whose turns run, and of those whose backend is still
    /// being shut down ([`TabSet::note_retiring`]). Reads only the tabs and touches no disk: the
    /// presence thread does that.
    fn sync_presence(&self) {
        if let Some(holder) = &self.presence {
            let mut running: BTreeSet<u64> = self
                .tabs
                .iter()
                .filter(|tab| tab.turn_running())
                .map(|tab| tab.id.0)
                .collect();
            running.extend(self.retiring.keys().copied());
            holder.sync(&running);
        }
    }

    /// A live backend of `tab` was taken away to be shut down elsewhere: until that shutdown has
    /// returned ([`TabSet::retire_done`]), the window's presence stays as if a turn of `tab` ran, so
    /// no window reverts a file the agent may still be writing. Every backend counts; Claude's
    /// shutdown is simply quick. The set's own `remove`, `reset` and tripwire call it; a caller that
    /// takes a live backend out of a tab itself calls it too.
    pub fn note_retiring(&mut self, tab: TabId) {
        *self.retiring.entry(tab.0).or_insert(0) += 1;
        self.sync_presence();
    }

    /// One shutdown [`TabSet::note_retiring`] counted for `tab` has returned. A tab with none
    /// outstanding is left alone.
    pub fn retire_done(&mut self, tab: TabId) {
        if let Some(count) = self.retiring.get_mut(&tab.0) {
            *count -= 1;
            if *count == 0 {
                self.retiring.remove(&tab.0);
            }
        }
        self.sync_presence();
    }

    /// The installed turn review, for building review jobs.
    pub fn turn_review(&self) -> Option<&crate::turn_review::TurnReview> {
        self.turn_review.as_ref()
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

    /// Rules apply to every tab from the next pump. A rule added after a card appeared does not
    /// answer that card.
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
        let used: Vec<u16> = self.tabs.iter().map(|t| t.number).collect();
        let id = TabId(self.next_id);
        self.next_id += 1;
        let number = tabs::lowest_free_number(&used);
        let mode = if told {
            self.mode_offered_to(&agent, self.default_mode, number)
        } else {
            agent.offered_mode(self.default_mode)
        };
        self.tabs.push(Tab::new(id, number, mode, agent));
        self.tabs.sort_by_key(|t| t.number);
        self.select(id);
        id
    }

    pub fn select(&mut self, id: TabId) -> bool {
        if self.get(id).is_none() {
            return false;
        }
        if id != self.active {
            self.last_active = Some(self.active);
            self.active = id;
            self.note_recent(id);
            // Changing the active tab drops its pending bypass prompt. The prompt refers to the tab
            // being left, and a `y` typed after switching must never answer it.
            self.drop_bypass_prompt();
        }
        true
    }

    fn note_recent(&mut self, id: TabId) {
        self.recency.retain(|t| *t != id);
        self.recency.push(id);
    }

    pub fn select_number(&mut self, number: u16) -> Option<TabId> {
        let id = self.tabs.iter().find(|t| t.number == number)?.id;
        self.select(id);
        Some(id)
    }

    /// `n` (+1) / `p` (-1), in number order, wrapping.
    pub fn step(&mut self, delta: i32) -> Option<TabId> {
        let numbers: Vec<u16> = self.tabs.iter().map(|t| t.number).collect();
        let next = tabs::step(&numbers, self.active_tab().number, delta)?;
        self.select_number(next)
    }

    pub fn select_last(&mut self) -> Option<TabId> {
        let last = self.last_active.filter(|id| self.get(*id).is_some())?;
        self.select(last);
        Some(last)
    }

    /// Takes a tab out and hands it back: its backend, pending start or handoff are the caller's to
    /// shut down or watch. A live backend is counted as retiring ([`TabSet::note_retiring`]): the
    /// caller reports [`TabSet::retire_done`] once its shutdown has returned. The last tab's removal
    /// opens a fresh tab 1.
    pub fn remove(&mut self, id: TabId) -> Option<Tab> {
        let at = self.tabs.iter().position(|t| t.id == id)?;
        let before = self.active;
        let mut tab = self.tabs.remove(at);
        // First, before anything that can take time: a write that has not reached its last check
        // must see that its tab is gone.
        tab.cancel_review_watchers();
        if let Some(review) = self.turn_review.as_mut() {
            review.close_tab(id.0, wall_clock_ms());
        }
        if matches!(tab.backend, TabBackend::Live(_)) {
            // Before the sync below, so the presence never drops between the turn and the shutdown.
            *self.retiring.entry(id.0).or_insert(0) += 1;
        }
        self.sync_presence();
        self.removed_arrived += tab.attention.attention().arrived;
        self.recency.retain(|t| *t != id);
        if self.last_active == Some(id) {
            self.last_active = None;
        }
        if self.tabs.is_empty() {
            self.open(AgentId::Claude);
            self.last_active = None;
        } else if self.active == id {
            let fallback = self.tabs[at.min(self.tabs.len() - 1)].id;
            self.active = self.last_active.take().unwrap_or(fallback);
            let now_active = self.active;
            self.note_recent(now_active);
        }
        // `open()` above already dropped it through `select`; this covers the fallback path, which
        // assigns `self.active` directly.
        if self.active != before {
            self.drop_bypass_prompt();
        }
        Some(tab)
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
    /// ended by stopping the session. `shown` is the count the question named; when the tab's count is
    /// another now, nothing is stopped and the user is asked to look again, since the answer was to a
    /// different question. Every refusal is benign and leaves the session as it was: no such tab, no
    /// live session, a turn running (an interrupt stops what it left), nothing left running.
    pub fn stop_background(&mut self, id: TabId, shown: u32) -> Result<Vec<AgentDomainEvent>, BackendError> {
        let benign = |message: String| BackendError {
            message,
            benign: true,
            folded_events: Vec::new(),
        };
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
                "a turn is running: interrupting it also stops what it left running".to_string(),
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
    pub fn cycle_mode(&mut self, id: TabId) -> Result<ModeCycle, String> {
        // A fresh press supersedes the open prompt. `Confirm` below stores a new one if it builds one.
        self.pending_bypass = None;
        self.pending_restore = None;
        self.drop_trust_questions();
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
                    _ if id != self.active => Err(format!("tab {} is not the one on screen", tab.number)),
                    _ => {
                        let scope_kind = if not_started {
                            tabs::PromptScope::EmptyTab
                        } else {
                            tabs::PromptScope::LiveTab
                        };
                        let approve = waiting_cards(tab);
                        let nonce = self.next_nonce;
                        self.next_nonce += 1;
                        let plan = BypassPlan {
                            scope: BypassScope::Tab(id),
                            nonce,
                            lines: vec![tabs::bypass_prompt(scope_kind, approve.len())],
                            approve,
                            prompt: scope_kind,
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
    pub fn cycle_default_mode(&mut self) -> ModeCycle {
        self.pending_bypass = None;
        self.pending_restore = None;
        self.drop_trust_questions();
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
                };
                self.pending_bypass = Some(plan.clone());
                ModeCycle::Confirm(plan)
            }
        }
    }

    /// Accept `y`/`Y` for a `confirm_bypass` prompt. Check the stored plan before taking it:
    /// a mismatched scope or stale nonce leaves the current prompt untouched, so an old answer
    /// cannot cancel a newer prompt.
    pub fn confirm_bypass(&mut self, scope: BypassScope, nonce: u64) -> Result<ConfirmOutcome, String> {
        match &self.pending_bypass {
            Some(plan) if plan.scope == scope && plan.nonce == nonce => {}
            _ => return Err("that prompt is no longer current".to_string()),
        }
        let plan = self.pending_bypass.take().expect("checked above");
        match scope {
            BypassScope::Tab(id) => {
                let Some(at) = self.tabs.iter().position(|t| t.id == id) else {
                    return Err(format!("no tab {}", id.0));
                };
                if id != self.active {
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
                // Recompute what `y` would approve now. A card delivered after the prompt was built must
                // never be silently approved.
                let now = waiting_cards(&self.tabs[at]);
                // Recheck which question the prompt asks: a terminal handoff can turn a live tab into
                // `NotStarted` while it is open. Confirming an empty tab also changes the window default, which
                // the live-tab prompt did not ask permission to change.
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
                    };
                    self.pending_bypass = Some(fresh.clone());
                    return Ok(ConfirmOutcome::Reprompt(fresh));
                }
                // `now` is already a subset of `plan.approve`, so it is their intersection. Only cards
                // that are still pending and delivered are approved.
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
                tab.host_answered.extend(answered.ids.iter().cloned());
                // The CLI's own prompts it answered say so on their calls' rows once those complete.
                note_answered_prompts(&mut tab.prompt_note_candidates, answered.prompts);
                let answered_set: BTreeSet<String> = answered.ids.iter().cloned().collect();
                tab.attention.retain_pending(|id| !answered_set.contains(id));
                let approved = answered.ids.len();
                let number = tab.number;
                eprintln!("[permission] mode is now bypass (tab {number}, approved {approved} waiting)");
                Ok(ConfirmOutcome::Entered {
                    approved,
                    resolved: answered.events,
                })
            }
            BypassScope::Default => {
                self.default_mode = SessionModeChoice::Bypass;
                eprintln!("[permission] mode is now bypass (new sessions)");
                Ok(ConfirmOutcome::Entered {
                    approved: 0,
                    resolved: Vec::new(),
                })
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
    /// gone or holds no live backend.
    pub fn answer_card(
        &mut self,
        id: TabId,
        permission_id: &str,
        decision: agent::PermissionDecision,
    ) -> Result<Vec<AgentDomainEvent>, BackendError> {
        let no_session = || BackendError {
            message: "no active session".to_string(),
            benign: true,
            folded_events: Vec::new(),
        };
        let tab = self.get_mut(id).ok_or_else(no_session)?;
        let backend = tab.live_mut().ok_or_else(no_session)?;
        // The call exactly as the card showed it: id, tool and input (the review's tightening).
        let call: Option<(String, String, serde_json::Value)> = backend
            .projection()
            .pending_permissions
            .get(permission_id)
            .and_then(|p| {
                let id = p.tool_use_id.clone().filter(|id| !id.is_empty())?;
                Some((id, p.tool_name.clone(), p.input.clone()))
            });
        let approves = decision.allows();
        let events = backend.respond_permission(permission_id, decision)?;
        if approves {
            if let Some((tool_use_id, tool_name, input)) = call {
                tab.human_allowed.record(&tool_use_id, &tool_name, &input);
            }
        }
        // A successful answer removes the rule offer and adds the id to `user_answered`, preventing
        // a new offer for this card. A failed answer leaves both the card and its offer open.
        tab.rule_offers.remove(permission_id);
        tab.user_answered.insert(permission_id.to_string());
        Ok(events)
    }

    /// Clears the current "enter bypass" prompt. Methods that change the active tab call this
    /// after the id changes, so a stale `y` cannot answer a prompt about a tab no longer on screen.
    /// Also called when the panel loses focus.
    pub fn drop_bypass_prompt(&mut self) {
        self.pending_bypass = None;
        self.pending_restore = None;
        self.drop_trust_questions();
    }

    /// Takes the question off every tab that waits for the trust answer: the tab keeps its start and
    /// is asked again, with a fresh nonce, when it is next on screen. An answer to the old nonce is
    /// refused from here on.
    fn drop_trust_questions(&mut self) {
        for tab in &mut self.tabs {
            if let TabBackend::AwaitingTrust(waiting) = &mut tab.backend {
                waiting.asking = None;
            }
        }
    }

    /// Puts a start off until the trust question is answered. Only an empty tab can wait: a tab with
    /// a session, one that is connecting and one already waiting each have their own start.
    pub fn defer_start(&mut self, tab: TabId, start: DeferredStart) -> Result<(), String> {
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

    /// Whether an answer carrying `nonce` may be taken for `tab` now: the tab is on screen, waiting,
    /// not already recording, and the question it shows is this one. Changes nothing.
    pub fn check_trust_answer(&self, tab: TabId, nonce: u64) -> Result<(), String> {
        let t = self.get(tab).ok_or_else(|| format!("no tab {}", tab.0))?;
        if tab != self.active {
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
    pub fn mark_recording(&mut self, tab: TabId, nonce: u64) -> Result<(), String> {
        self.check_trust_answer(tab, nonce)?;
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
    pub fn take_answered(&mut self, tab: TabId, nonce: u64) -> Result<DeferredStart, String> {
        self.check_trust_answer(tab, nonce)?;
        Ok(self.take_start(tab).expect("checked to be waiting"))
    }

    /// The start whose `y` the worker confirmed, even when the user has since moved to another tab.
    pub fn take_recorded(&mut self, tab: TabId) -> Option<DeferredStart> {
        match &self.get(tab)?.backend {
            TabBackend::AwaitingTrust(AwaitingTrust { recording: true, .. }) => self.take_start(tab),
            _ => None,
        }
    }

    /// The route that asks nothing (the answer is already known): hands the start over. Refused
    /// while a record is being written, whose result decides.
    pub fn take_deferred(&mut self, tab: TabId) -> Option<DeferredStart> {
        match &self.get(tab)?.backend {
            TabBackend::AwaitingTrust(AwaitingTrust { recording: false, .. }) => self.take_start(tab),
            _ => None,
        }
    }

    /// `Escape` on the question: puts the start off and returns the tab to empty. The same checks as
    /// an answer.
    pub fn cancel_trust(&mut self, tab: TabId, nonce: u64) -> Result<DeferredStart, String> {
        self.check_trust_answer(tab, nonce)?;
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
    pub fn reset(&mut self, id: TabId) -> Result<Option<AgentBackend>, String> {
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let waiting = matches!(tab.backend, TabBackend::AwaitingTrust(_));
        match tab.wire_state() {
            TabStateWire::Ended | TabStateWire::Failed => {}
            // A tab waiting for the trust question starts over too: its put-off start is dropped.
            _ if waiting => {}
            _ => return Err("only an ended or failed tab starts over".to_string()),
        }
        // Restore the queue and unsent draft as the draft, oldest first. A deferred first message
        // comes before them because it was typed first.
        let mut parts: Vec<String> = Vec::new();
        if let TabBackend::AwaitingTrust(AwaitingTrust {
            start: DeferredStart::FirstSend { first_turn, .. },
            ..
        }) = &tab.backend
        {
            parts.push(first_turn.typed.clone());
        }
        parts.extend(std::mem::take(&mut tab.queue).into_iter().map(|q| q.text));
        if !tab.draft.trim().is_empty() {
            parts.push(std::mem::take(&mut tab.draft));
        }
        tab.draft = parts.join("\n\n");
        tab.queue_error = None;
        tab.queue_note = None;
        tab.rule_offers.clear();
        tab.rule_offers_seen.clear();
        tab.user_answered.clear();
        tab.was_running = false;
        let old = std::mem::replace(&mut tab.backend, TabBackend::NotStarted);
        // An empty tab shows what its next session would get, not what the last one had.
        tab.project_tiers = None;
        tab.title = None;
        tab.title_from_record = false;
        tab.attention.restart();
        tab.stale = false;
        tab.turn_trace = None;
        tab.reported_start_failure = false;
        // The backend that answered these is gone with `old`, so there is nothing left to hide.
        // `tab.mode` stays unchanged: a bypass tab resets to an empty bypass tab.
        tab.host_answered.clear();
        // Resetting an ended or failed tab to `NotStarted` invalidates the open bypass prompt.
        // Drop it so `y` cannot answer a question about the tab's previous state. `confirm_bypass` also
        // rechecks the tab kind before acting.
        tab.human_allowed.clear();
        tab.prompt_note_candidates.clear();
        tab.auto_edit_candidates.clear();
        tab.creates_file_unsent.clear();
        self.pending_bypass = None;
        Ok(match old {
            TabBackend::Live(backend) => {
                self.note_retiring(id);
                Some(backend)
            }
            _ => None,
        })
    }

    /// Resolve an explicit tab reference; reject missing or unknown tab ids.
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

    /// Every Claude session this window has open or just handed off: filtered out of `hello`
    /// and of the chooser's records.
    pub fn open_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.tabs.iter().filter_map(Tab::provider_session_id).collect();
        ids.extend(
            self.tabs
                .iter()
                .filter_map(|t| t.last_handoff.as_ref().map(|c| c.provider_session_id().to_string())),
        );
        ids
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
            self.select(open);
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
        ResumeRoute::StartIn(target)
    }

    /// One tick over EVERY tab, run when something is due (at most one per 33 ms). The active tab's delivery is serialized for the
    /// panel; a background tab's feeds its attention and marks it stale, and nothing of it is sent.
    pub fn pump(&mut self, project_root: &Path, panel_mapped: bool) -> PumpOutput {
        let active = self.active;
        let mut out = PumpOutput {
            active_payload: None,
            active_class: crate::panel_cadence::EnvelopeClass::Immediate,
            first_text: false,
            turn_ended: Vec::new(),
            offers_changed: Vec::new(),
            queue_changed: Vec::new(),
            tripped: Vec::new(),
            review_hints: Vec::new(),
        };
        // A turn a send started since the last pump (a legacy turn begins this way) is seen here.
        self.sync_presence();
        // Before this tick's events: a base that landed by now is not pending for them.
        if let Some(review) = self.turn_review.as_mut() {
            out.review_hints = review.poll();
        }
        let rules = &self.rules;
        let ui_wake = self.ui_wake.as_ref();
        let mut review = self.turn_review.as_mut();
        for tab in &mut self.tabs {
            let is_active = tab.id == active;
            let on_screen = panel_mapped && is_active;
            let TabBackend::Live(backend) = &mut tab.backend else {
                if let Some(review) = review.as_deref_mut() {
                    review.tab_has_no_session(tab.id.0, wall_clock_ms());
                }
                // No backend holds no card: the backstop `agent_panel` had for `session.is_none()`.
                tab.attention.retain_pending(|_| false);
                tab.was_running = false;
                tab.turn_clock = None;
                // The backend that answered these on the user's behalf is gone; nothing left to hide.
                tab.host_answered.clear();
                tab.human_allowed.clear();
                tab.prompt_note_candidates.clear();
                tab.auto_edit_candidates.clear();
                tab.creates_file_unsent.clear();
                tab.user_answered.clear();
                continue;
            };
            // A backend put in place some other way than `collect_starts` is told about the wake here.
            if let Some(wake) = ui_wake {
                backend.ensure_ui_wake(wake);
            }
            let mut turn_ended = false;
            // Whether this tab's payload goes to the panel this tick (then `shell` prints a complete
            // trace after dispatching it; otherwise this function does, below).
            let mut dispatched = false;
            // `tab.mode`: the classifier's `Auto` path, or bypass answering
            // everything, and `tab.host_answered` collects every id either path answers so the
            // Resync arm and `active_state_payloads` can hide them.
            // `tab.human_allowed`: in `Auto`, the CLI's own prompt for exactly a call
            // the user approved on a card here is answered without a second card, once. What was
            // answered without a card becomes a note on its call's row when the call completes.
            let mut answered_for_you = crate::agent_backend::AnsweredForYou::default();
            let delivery = backend.take_revised_ui_delivery(
                project_root,
                rules,
                tab.mode.into(),
                &mut tab.host_answered,
                &mut tab.human_allowed,
                &mut answered_for_you,
            );
            #[cfg(test)]
            {
                if let Some(hook) = self.after_drain.take() {
                    hook(backend);
                }
            }
            // Take the revision covered by the last snapshot sent for this backend. The drain and snapshot
            // use separate locks, so ingestion may fold and queue an event between them, leaving it in
            // both the snapshot and this drain. Each event carries its fold's revision. Folding, queueing
            // and reading the snapshot revision share one ingestion mutex, so every queued event at or
            // below that revision was covered and is present in this drain. Take the revision now so no
            // later drain reuses it. `AgentBackend::note_ui_snapshot` keeps it with the backend so it
            // cannot outlive the session.
            let covered = backend.take_ui_snapshot_revision();
            #[cfg(test)]
            let covered = covered.filter(|_| !self.redeliver_covered);
            #[cfg(any(test, feature = "test-support"))]
            tab.gate_log.extend(answered_for_you.gate_answers);
            note_answered_prompts(&mut tab.prompt_note_candidates, answered_for_you.prompts);
            // Keep fast-path answers, saved-rule answers, and `Write` answers for nonexistent files as
            // captured when answered. The first two wait for call completion (`note_auto_edit_answers`,
            // `note_rule_answers`); file creation markers need only a payload to carry them
            // (`creates_file_unsent`).
            tab.auto_edit_candidates
                .extend(answered_for_you.by_the_fast_path.into_iter().map(|a| {
                    (
                        a.tool_use_id,
                        CandidateCall {
                            tool_name: a.tool_name,
                            input: a.input,
                        },
                    )
                }));
            tab.rule_candidates
                .extend(answered_for_you.by_rule.into_iter().map(|a| {
                    (
                        a.tool_use_id,
                        RuleCandidate {
                            rule: a.rule,
                            call: CandidateCall {
                                tool_name: a.tool_name,
                                input: a.input,
                            },
                        },
                    )
                }));
            for id in answered_for_you.creates_file {
                if tab.notes.auto_creates_file.insert(id.clone()) {
                    tab.creates_file_unsent.insert(id);
                }
            }
            // The CLI-mode tripwire, before anything of this delivery reaches the
            // panel or the attention count: from the batch itself, or -- for a report a `Resync`
            // dropped, or one folded ahead of its delivery -- from the projection's record. The
            // whole batch is withheld, so no card in it is ever drawn to be approved; the session
            // is handed out to be shut down, which denies what is still pending in it.
            let tripped_reason = match &delivery {
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
            let tripped_reason = tripped_reason.or_else(|| {
                let projection = backend.projection();
                if let Some(r) = &projection.ungated_cli_mode {
                    return Some(ungated_cli_mode_reason(&r.reported, &r.detail));
                }
                projection
                    .policy_mismatch
                    .as_ref()
                    .map(|(requested, reported)| policy_mismatch_reason(requested, reported))
            });
            if let Some(reason) = tripped_reason {
                eprintln!("[permission] tab {}: {reason}", tab.id.0);
                // Nothing will report on a message still in the turn once the session goes.
                if tab.hold_steered_on_failure() {
                    out.queue_changed.push(tab.id);
                }
                tab.attention.session_ended(on_screen);
                tab.was_running = false;
                tab.turn_clock = None;
                tab.failed_after_start = true;
                tab.failed_resume = None;
                let dead = std::mem::replace(&mut tab.backend, TabBackend::Failed { reason: reason.clone() });
                if let TabBackend::Live(backend) = dead {
                    *self.retiring.entry(tab.id.0).or_insert(0) += 1;
                    out.tripped.push(Tripped {
                        tab: tab.id,
                        reason,
                        backend,
                    });
                }
                continue;
            }
            // Keeps `tab.turn_clock` in step for the `Events` arm, which builds its payload straight
            // from the drained batch and takes no further lock after this -- there is no later,
            // fresher read to prefer. The `Resync` arm is different (see its own comment below) and
            // re-observes the clock itself from the snapshot it actually sends, rather than reading
            // `tab.turn_clock` as this call left it.
            {
                let projection = backend.projection();
                observe_turn_clock(
                    &mut tab.turn_clock,
                    &mut tab.notes.turn_times,
                    &mut tab.turn_times_unsent,
                    &projection,
                    wall_clock_ms(),
                );
            }
            let mut resynced = false;
            match delivery {
                RevisedDelivery::Nothing => {}
                RevisedDelivery::Events(tagged) => {
                    // The tags strictly increase, so what the last snapshot already carried is a
                    // prefix of the batch: `events[..shown]` reached the panel inside that snapshot,
                    // `events[shown..]` has not reached it at all. Everything below folds the whole
                    // batch into this tab's own bookkeeping -- none of it has been seen here yet --
                    // and only the payload leaves the prefix out.
                    let shown = covered.map_or(0, |through| {
                        tagged.partition_point(|(revision, _)| *revision <= through)
                    });
                    // The envelope's bracket, from the events' own fold revisions: the revision
                    // before the first one it carries, and that of the batch's last.
                    let through_revision = tagged.last().map_or(0, |(revision, _)| *revision);
                    let from_revision = tagged
                        .get(shown)
                        .map_or(through_revision, |(revision, _)| revision.saturating_sub(1));
                    let events: Vec<AgentDomainEvent> = tagged.into_iter().map(|(_, event)| event).collect();
                    let (in_snapshot, fresh) = events.split_at(shown);
                    for event in &events {
                        match event {
                            AgentDomainEvent::TurnStarted { .. } => tab.was_running = true,
                            // An interrupt's end and the legacy backend's synthesized end are
                            // `TurnCompleted`s too. Reported whatever `was_running` says: a legacy
                            // `TurnStarted` is returned by `send_turn` and never pumped.
                            AgentDomainEvent::TurnCompleted { .. } => {
                                tab.was_running = false;
                                turn_ended = true;
                                // The CLI asks within the turn that made the call (O3).
                                tab.human_allowed.clear();
                            }
                            // The session ended: its queue waits for `r`, not a flush.
                            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. } => {
                                tab.was_running = false;
                            }
                            // `Tab::mode` is authoritative; provider reports do not change it. A `PermissionModeChanged`
                            // that reaches here names `default`; other modes became `UngatedCliMode` above.
                            //
                            // A resolution removes the id previously answered on the user's behalf from `host_answered`,
                            // since it no longer needs to be hidden.
                            AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                                tab.host_answered.remove(permission_id);
                            }
                            _ => {}
                        }
                    }
                    // The whole batch, what the snapshot already carried included: the review has
                    // seen neither part. Only enqueues.
                    if let Some(review) = review.as_deref_mut() {
                        let session = backend.provider_session_id().unwrap_or_default();
                        let now = wall_clock_ms();
                        for event in &events {
                            review.observe(tab.id.0, &session, event, now);
                        }
                    }
                    let fresh_notes = CallNotes {
                        allowed_by_rule: note_rule_answers(
                            &mut tab.rule_candidates,
                            &mut tab.notes.allowed_by_rule,
                            &events,
                        ),
                        creates_file: note_new_files(&mut tab.notes.creates_file, &events, project_root),
                        prompt_notes: note_prompt_answers(
                            &mut tab.prompt_note_candidates,
                            &mut tab.notes.prompt_notes,
                            &events,
                        ),
                        allowed_by_auto: note_auto_edit_answers(
                            &mut tab.auto_edit_candidates,
                            &mut tab.notes.allowed_by_auto,
                            &events,
                        ),
                        auto_creates_file: std::mem::take(&mut tab.creates_file_unsent),
                        // Only the active tab's payload leaves here; a background tab's turns wait
                        // for the snapshot its next switch sends.
                        turn_times: if is_active {
                            let times = &tab.notes.turn_times;
                            let fresh = times[times.len() - tab.turn_times_unsent.min(times.len())..].to_vec();
                            tab.turn_times_unsent = 0;
                            fresh
                        } else {
                            Vec::new()
                        },
                    };
                    // Every card in the batch is counted, those the snapshot drew included: they are
                    // pending all the same, and this tab has not counted them yet.
                    tab.attention.observe(&events, on_screen);
                    if let Some(trace) = tab.turn_trace.as_mut() {
                        if !in_snapshot.is_empty() {
                            trace.observe_in_snapshot(in_snapshot);
                        }
                        let first_text = trace.observe(fresh);
                        if is_active {
                            out.first_text |= first_text;
                        } else if first_text {
                            trace.mark_background();
                        }
                    }
                    if is_active {
                        // Nothing to send when the snapshot carried the whole batch -- unless the
                        // batch produced notes, which that snapshot (serialized before this tab had
                        // folded these events) did not carry; the panel applies notes to what it
                        // already holds (`applyCallNotes`), so they go on their own.
                        if !fresh.is_empty() || !fresh_notes.is_empty() {
                            out.active_class = crate::panel_cadence::classify_events(fresh, !fresh_notes.is_empty());
                            out.active_payload = Some(serialize_events_with_notes_for_js(
                                tab.id,
                                from_revision,
                                through_revision,
                                fresh,
                                &fresh_notes,
                            ));
                            dispatched = true;
                        }
                    } else {
                        tab.stale = true;
                    }
                }
                RevisedDelivery::Resync => {
                    resynced = true;
                    // The events are gone; show the panel a snapshot.
                    //
                    // The drain and `SnapshotView::of` take separate `Mutex<IngestState>` locks, with a bypass
                    // `approve_pending` lock cycle between them because answering must happen outside the lock.
                    // A batch folded in this gap is queued for the next tick and already in this snapshot.
                    // Read the snapshot revision under its serialization guard and note it on the backend, so
                    // the next drain omits that batch from the payload, as after `active_state_payloads`.
                    let running = backend.projection().active_turn_id.is_some();
                    turn_ended = tab.was_running && !running;
                    tab.was_running = running;
                    // Unconditionally (Codex's finding): the dropped events may have held a turn's
                    // end and the next one's start, which `was_running` cannot see, and an approval
                    // must never outlive its turn. Costs at most a card.
                    tab.human_allowed.clear();
                    // A candidate whose call finished inside the dropped events becomes its note now,
                    // so the snapshot below already carries it.
                    {
                        let projection = backend.projection();
                        let finished: Vec<String> = tab
                            .prompt_note_candidates
                            .keys()
                            .filter(|id| {
                                projection
                                    .tool_calls
                                    .iter()
                                    .any(|call| &call.tool_use_id == *id && call.result.is_some())
                            })
                            .cloned()
                            .collect();
                        for id in finished {
                            if let Some(note) = tab.prompt_note_candidates.remove(&id) {
                                tab.notes.prompt_notes.insert(id, note);
                            }
                        }
                        // A fast-path answer is captured when given. If its call completed among dropped events,
                        // convert its candidate into a note here so it cannot linger indefinitely.
                        let finished: Vec<String> = tab
                            .auto_edit_candidates
                            .keys()
                            .filter(|id| {
                                projection
                                    .tool_calls
                                    .iter()
                                    .any(|call| &call.tool_use_id == *id && call.result.is_some())
                            })
                            .cloned()
                            .collect();
                        for id in finished {
                            tab.auto_edit_candidates.remove(&id);
                            tab.notes.allowed_by_auto.insert(id);
                        }
                        // A rule's answer is captured when given. If its call completed among dropped events,
                        // convert its candidate into a note here too. The still-pending sweep below cannot remove a
                        // candidate whose call has already finished, so without this sweep it would linger indefinitely.
                        let finished: Vec<String> = tab
                            .rule_candidates
                            .keys()
                            .filter(|id| {
                                projection
                                    .tool_calls
                                    .iter()
                                    .any(|call| &call.tool_use_id == *id && call.result.is_some())
                            })
                            .cloned()
                            .collect();
                        for id in finished {
                            if let Some(candidate) = tab.rule_candidates.remove(&id) {
                                tab.notes.allowed_by_rule.insert(id, candidate.rule);
                            }
                        }
                        // The snapshot below carries every note, these included.
                        tab.creates_file_unsent.clear();
                    }
                    // A dropped `PermissionRequested` can leave a candidate whose call has not finished, while
                    // the projection still shows its permission awaiting a human. The completed-call sweep above
                    // cannot remove it. `answer_what_needs_no_human` never runs on a dropped batch, so a real card
                    // will be drawn from the snapshot. Remove its candidate instead of making a note, or a later
                    // human approval would be mislabeled "allowed by auto".
                    //
                    // `rule_candidates` can lose the same removal event in `note_rule_answers`. Remove those by id
                    // too, or a human approval could be mislabeled "allowed by rule".
                    //
                    // Exclude requests already in `host_answered`: they remain pending until the provider folds
                    // `PermissionResolved`, but the snapshot hides their cards. Sweeping them would lose an answer
                    // merely because `Resync` arrived before its resolution. Auto candidates exist only after the
                    // fast path answered the gate, so an unanswered card for one is the CLI's later prompt for the
                    // same call.
                    // The same applies to rule candidates: they exist only after a rule answered the gate, and a
                    // later unanswered card is the CLI's prompt for that call. Both candidate maps need the
                    // `host_answered` filter to retain answers until call completion while provider resolutions
                    // are still pending.
                    {
                        let still_pending: BTreeSet<String> = backend
                            .projection()
                            .pending_permissions
                            .values()
                            .filter(|p| !tab.host_answered.contains(&p.permission_id))
                            .filter_map(|p| p.tool_use_id.clone())
                            .collect();
                        tab.auto_edit_candidates.retain(|id, _| !still_pending.contains(id));
                        tab.rule_candidates.retain(|id, _| !still_pending.contains(id));
                    }
                    let pending: Vec<String> = backend.projection().pending_permissions.keys().cloned().collect();
                    // In bypass, resync sends pending requests lost to overflow through `approve_pending`,
                    // as confirmation does, so rebuilding the panel never creates cards for those requests.
                    // Also filter out requests already answered by the classifier in `Auto`.
                    if tab.mode == SessionModeChoice::Bypass {
                        let sweep: Vec<String> = pending
                            .iter()
                            .filter(|id| !tab.host_answered.contains(*id))
                            .cloned()
                            .collect();
                        // The events (legacy's own resolutions) are not needed here: the snapshot below
                        // is read after they were folded, so it already shows those cards answered.
                        let answered = backend.approve_pending(&sweep, "in bypass after a resync");
                        tab.host_answered.extend(answered.ids);
                        note_answered_prompts(&mut tab.prompt_note_candidates, answered.prompts);
                    }
                    tab.attention
                        .resync(pending.iter().filter(|id| !tab.host_answered.contains(*id)).cloned());
                    if is_active {
                        #[cfg(test)]
                        {
                            if let Some(hook) = self.before_resync_snapshot.take() {
                                hook(backend);
                            }
                        }
                        let (payload, revision) = {
                            let view = SnapshotView::of(backend, &tab.host_answered);
                            // Read the turn clock from this snapshot's view under its serialization guard. The earlier
                            // read above `match delivery` can be stale after this arm's `projection()` calls and bypass
                            // sweep. If a turn ends and another starts in that gap, the early read still names the old
                            // turn while the snapshot names the new one. The panel treats the stamped time as exact,
                            // so use the same guarded observation as `active_state_payloads`.
                            observe_turn_clock(
                                &mut tab.turn_clock,
                                &mut tab.notes.turn_times,
                                &mut tab.turn_times_unsent,
                                &view.projection,
                                wall_clock_ms(),
                            );
                            let payload = serialize_snapshot_with_notes_for_js(
                                tab.id,
                                &view,
                                tab.turn_clock.as_ref().map(|(_, at)| *at),
                                &tab.notes,
                            );
                            tab.turn_times_unsent = 0;
                            // Under the guard the snapshot is serialized from (see the arm's comment).
                            (payload, view.projection.last_revision)
                        };
                        backend.note_ui_snapshot(revision);
                        out.active_payload = Some(payload);
                        // A snapshot replaces the panel's state: never held behind a cadence slot.
                        out.active_class = crate::panel_cadence::EnvelopeClass::Immediate;
                        dispatched = true;
                    } else {
                        tab.stale = true;
                    }
                }
            }
            // After the resync, not inside it: the review learns from the rebuilt projection what
            // the dropped events would have told it. Each read in its own statement (lock order).
            if resynced {
                if let Some(review) = review.as_deref_mut() {
                    let session = backend.provider_session_id().unwrap_or_default();
                    let running = backend.projection().active_turn_id.clone();
                    let background = backend.projection().background_calls;
                    let at_end = backend.projection().background_at_last_turn_end;
                    let now = wall_clock_ms();
                    // A turn the resync ends ended with the count it had then, not with the one now:
                    // the command may have finished among the dropped events.
                    let count = |running| AgentDomainEvent::BackgroundCalls { running };
                    review.observe(tab.id.0, &session, &count(at_end), now);
                    review.observe_resync(tab.id.0, &session, running.as_deref(), now);
                    review.observe(tab.id.0, &session, &count(background), now);
                }
            }
            // Print a completed trace here if there is no dispatch to wait for: a background tab, or an
            // active tab with no panel delivery this tick. Its end may have arrived only in a snapshot.
            // `shell` prints dispatched traces after stamping their dispatch.
            if !dispatched {
                if let Some(trace) = tab.turn_trace.as_mut() {
                    if trace.is_complete() {
                        trace.emit();
                    }
                }
            }
            let still: std::collections::HashSet<String> =
                backend.projection().pending_permissions.keys().cloned().collect();
            tab.attention.retain_pending(|id| still.contains(id));
            tab.host_answered.retain(|id| still.contains(id));
            tab.user_answered.retain(|id| still.contains(id));
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
            if turn_ended && !tab.turn_running() {
                out.turn_ended.push(tab.id);
            }
        }
        // A `TurnStarted` folded in this pump.
        self.sync_presence();
        out
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
                    folded_events: Vec::new(),
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
                    tab.was_running = false;
                    tab.attention.restart();
                    tab.notes = CallNotes::default();
                    // A new session numbers its turns afresh, and the old tab's last turn was not
                    // seen to end: its clock and durations belong to the session that is gone.
                    tab.turn_clock = None;
                    tab.turn_times_unsent = 0;
                    tab.rule_candidates.clear();
                    tab.auto_edit_candidates.clear();
                    tab.creates_file_unsent.clear();
                    tab.prompt_note_candidates.clear();
                    tab.last_handoff = None;
                    tab.reported_start_failure = false;
                    tab.title = pending.resumed_title;
                    // Only Claude's sessions have a transcript the CLI titles.
                    tab.title_from_record = tab.title.is_some() && tab.agent == AgentId::Claude;
                    // A freshly installed backend has answered nothing yet on this tab's behalf.
                    tab.host_answered.clear();
                    tab.user_answered.clear();
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

    /// After a switch or `ready`, send the active tab's snapshot if it has a backend, or its
    /// handoff card if it has one, even when the tab is not stale.
    ///
    /// Read without draining. The sidecar's ingestion thread can fold and queue events after the
    /// last drain, so the snapshot may already cover queued events. Read its revision under the
    /// serialization guard and store it through `AgentBackend::note_ui_snapshot`. The next `pump`
    /// omits covered events from the panel payload but still applies them to the tab's turn end,
    /// attention, notes and trace. This leaves window attention unchanged here; `shell` reports
    /// it around ticks and switches, but not panel reloads. A request folded in the roughly 33 ms
    /// before a switch or reload can still draw a card until the next tick answers it and its
    /// resolution arrives.
    pub fn active_state_payloads(&mut self) -> Vec<String> {
        let tab = self.active_tab_mut();
        tab.stale = false;
        let mut payloads = Vec::new();
        match &mut tab.backend {
            TabBackend::Live(backend) => {
                let (payload, revision) = {
                    let view = SnapshotView::of(backend, &tab.host_answered);
                    // The clock follows the turn this very snapshot names: one that started since
                    // the last tick is stamped now, as that tick would have, and a clock left from
                    // the turn before it is never sent with this one's id (the panel trusts it as
                    // exact).
                    observe_turn_clock(
                        &mut tab.turn_clock,
                        &mut tab.notes.turn_times,
                        &mut tab.turn_times_unsent,
                        &view.projection,
                        wall_clock_ms(),
                    );
                    let payload = serialize_snapshot_with_notes_for_js(
                        tab.id,
                        &view,
                        tab.turn_clock.as_ref().map(|(_, at)| *at),
                        &tab.notes,
                    );
                    tab.turn_times_unsent = 0;
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
            &tab.queue,
            tab.queue_error.as_deref(),
            tab.queue_note.as_deref(),
            tab.steers(),
        ));
        payloads.push(crate::agent_bridge::serialize_draft_for_js(tab.id, &tab.draft));
        if tab.live().is_some() {
            payloads.push(crate::agent_bridge::serialize_rule_offers_for_js(
                tab.id,
                &tab.rule_offers,
            ));
        }
        payloads.push(crate::agent_bridge::serialize_scratch_for_js(
            tab.id,
            tab.editing_draft.is_some(),
        ));
        payloads
    }

    pub fn tabs_payload(&self) -> String {
        let views: Vec<TabView> = self.tabs.iter().map(|t| t.view(t.resumable(self.kind))).collect();
        serialize_tabs_for_js(self.active, &views, self.default_mode)
    }

    /// The active tab is on screen: what finished in it while away has been seen.
    pub fn mark_seen(&mut self) {
        self.active_tab_mut().attention.seen();
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

    /// Count tabs with a turn in progress or a connection still starting.
    pub fn running_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.backend, TabBackend::Starting(_)) || t.turn_running())
            .count()
    }

    /// `<leader>bd` asks before losing a running turn, a connect in flight or queued messages,
    /// the facts named by `close_prompt`. Like `:bd` with unsaved changes, it asks only when there
    /// is something to lose. `prefix &` always asks through `confirm-before`.
    pub fn close_needs_confirm(&self, id: TabId) -> bool {
        self.get(id).is_some_and(|t| {
            matches!(t.backend, TabBackend::Starting(_) | TabBackend::AwaitingTrust(_))
                || t.turn_running()
                || !t.queue.is_empty()
        })
    }

    pub fn close_facts(&self, id: TabId) -> Option<tabs::CloseFacts> {
        let tab = self.get(id)?;
        Some(tabs::CloseFacts {
            number: tab.number,
            label_name: tab.label_name(),
            turn_running: tab.turn_running(),
            queued: tab.queue.len() - tab.steered_unconfirmed(),
            steered: tab.steered_unconfirmed(),
            legacy: self.kind == BackendKind::Legacy && tab.agent == AgentId::Claude,
            has_backend: tab.live().is_some(),
        })
    }

    /// `<leader>bo`: the ids `tabs::close_others` would close and their y/n prompt, covering all
    /// tabs except the active one. Uses [`Self::running_count`]'s running test: a turn in progress or
    /// still connecting. Returns `None` when only the active tab is open.
    pub fn close_others_plan(&self) -> Option<(Vec<TabId>, String)> {
        let entries: Vec<(TabId, bool)> = self
            .tabs
            .iter()
            .map(|t| {
                (
                    t.id,
                    matches!(t.backend, TabBackend::Starting(_) | TabBackend::AwaitingTrust(_)) || t.turn_running(),
                )
            })
            .collect();
        tabs::close_others(&entries, self.active())
    }

    /// `wire` already includes the editor context captured when the message was composed.
    /// When `steerable` is false, the message waits for the turn's end regardless of later Enter presses.
    pub fn queue_message(
        &mut self,
        id: TabId,
        text: &str,
        wire: String,
        now_ms: u64,
        steerable: bool,
    ) -> Result<usize, String> {
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
        tab.queue.push(Queued {
            text: text.to_string(),
            wire,
            queued_at_ms: now_ms,
            state: QueuedState::Held,
            steerable,
            dropped_as: Vec::new(),
        });
        Ok(tab.queue.len())
    }

    /// Enter while a turn runs: the tab's held messages, oldest first, go into the running turn when its
    /// agent can take messages mid-turn (`AgentBackend::steer`); each leaves the queue once the agent
    /// reads it. Only into the turn the page showed running when Enter was pressed (`shown_turn`): a message
    /// typed in a turn that has ended since waits for the next end. The first refusal stops the rest, so a
    /// later message never reaches the agent before an earlier one (the provider keeps that order for the
    /// refusals it learns of later), and the held ones wait for the turn's end with the reason under them.
    /// A message that may not be steered (a confirmed review, or a slash command on an agent that reads
    /// commands: `reads_slash_commands`) stops them too, for the same order, and so does a message the
    /// provider reported dropped once (`Queued::dropped_as`): the agent may still read that copy. A card
    /// waiting does not stop it: the agent reads the message once that call is answered, sooner than the
    /// turn's end, and the card keeps its own keys. Whether the queue changed.
    pub fn steer_held(&mut self, id: TabId, now_ms: u64, shown_turn: Option<&str>) -> bool {
        let mut next = self.next_steer;
        let Some(tab) = self.get_mut(id) else { return false };
        let mut changed = tab.settle_steered();
        if tab.pending_handoff.is_some() || !tab.turn_running() || !tab.queue.iter().any(Queued::is_held) {
            return changed;
        }
        let label = tab.agent.label();
        let TabBackend::Live(backend) = &mut tab.backend else {
            return changed;
        };
        let reads_commands = reads_slash_commands(&tab.agent, &backend.projection());
        let mut refusal = None;
        for item in tab.queue.iter_mut().filter(|q| q.is_held()) {
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
            if reads_commands && is_slash_command(&item.text) {
                refusal = Some("a command waits for the turn's end, and what follows it waits too".to_string());
                break;
            }
            let Some(turn) = shown_turn else {
                refusal = Some("no turn was running when it was typed".to_string());
                break;
            };
            let client_id = format!("eitri-{}-{next}", id.0);
            match backend.steer(turn, &client_id, &item.wire, &item.text) {
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
        let note = refusal.as_deref().map(held_note);
        changed |= tab.queue_note != note;
        tab.queue_note = note;
        self.next_steer = next;
        changed
    }

    /// Send the held queue as one turn when the tab can accept it.
    ///
    /// A message sent into the previous turn has either had its outcome reported, been held again,
    /// or started a follow-up turn through `AgentProvider::steer_turn`. A running turn prevents
    /// another send. Messages with no reported outcome are retried, accepting possible duplicate
    /// delivery rather than silently losing them.
    ///
    /// Read delivery outcomes and the active turn under one projection lock. Separate reads could
    /// straddle a delivery and turn completion, making a read message look unreported and sending it twice.
    pub fn flush_queue(&mut self, id: TabId) -> Option<Flush> {
        let tab = self.get_mut(id)?;
        if tab.queue.is_empty() || tab.pending_handoff.is_some() {
            return None;
        }
        {
            let TabBackend::Live(backend) = &tab.backend else {
                return None;
            };
            let projection = backend.projection();
            settle_against(&mut tab.queue, &mut tab.queue_note, &projection);
            if projection.active_turn_id.is_some() || !projection.pending_permissions.is_empty() {
                return None;
            }
        }
        if tab.queue.is_empty() {
            return None;
        }
        if !tab.queue.iter().all(Queued::is_held) {
            eprintln!(
                "[agent_panel] tab {}: the agent never said whether it read a message sent into its turn; it is sent again",
                tab.id.0
            );
            for item in tab.queue.iter_mut() {
                item.state = QueuedState::Held;
            }
        }
        let wire = tab
            .queue
            .iter()
            .map(|q| q.wire.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let typed = tab
            .queue
            .iter()
            .map(|q| q.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let trace = crate::turn_trace::TurnTrace::start();
        let mode = tab.mode;
        let outcome = tab.live_mut().expect("checked above").send_turn_in(&wire, &typed, mode);
        match &outcome {
            Ok(_) => {
                tab.queue.clear();
                tab.queue_error = None;
                tab.queue_note = None;
                tab.turn_trace = trace;
            }
            Err(error) => tab.queue_error = Some(error.message.clone()),
        }
        tab.was_running = tab.turn_running();
        Some(Flush { typed, outcome })
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
        self.queue_message(id, text, text.to_owned(), now_ms, false)?;
        Ok(self.flush_queue(id))
    }

    /// Queue nonempty text, then interrupt a running turn or flush the queue.
    pub fn send_now(&mut self, id: TabId, text: &str, wire: String, now_ms: u64) -> Result<SendNow, String> {
        if !text.trim().is_empty() {
            self.queue_message(id, text, wire, now_ms, true)?;
        }
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        if tab.turn_running() {
            let backend = tab.live_mut().expect("a running turn has a backend");
            return Ok(SendNow::Interrupting(backend.interrupt()));
        }
        Ok(SendNow::Flushed(self.flush_queue(id)))
    }

    /// C5: `id` is the scratch request now holding `tab`'s draft.
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
        let draft = match done {
            crate::scratch::EditDone::Written(text) => {
                tab.draft = text.clone();
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
            let saved = match results.iter().find(|(result_id, _)| *result_id == id) {
                Some((_, crate::scratch::EditDone::Written(text)))
                    if *text != tab.draft && Some(text.as_str()) != tab.draft.strip_suffix('\n') =>
                {
                    tab.draft = text.clone();
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
        tab.queue_error = None;
        tab.queue_note = None;
        let (held, steered): (Vec<Queued>, Vec<Queued>) =
            std::mem::take(&mut tab.queue).into_iter().partition(Queued::is_held);
        tab.queue = steered;
        held.into_iter().map(|q| q.text).collect()
    }

    /// Mirror the panel's draft without echoing it back.
    pub fn set_draft(&mut self, id: TabId, text: &str) -> bool {
        let Some(tab) = self.get_mut(id) else { return false };
        tab.draft = text.to_string();
        true
    }

    /// Clear the mirrored draft when `id` sends a prompt. The composer clears on Enter but mirrors
    /// `""` only 300 ms later. `active_state_payloads` can run when the first connect finishes and
    /// send this stale copy back to the composer, restoring text already sent. Clear it here
    /// rather than waiting for the mirror.
    pub fn note_sent(&mut self, id: TabId) {
        if let Some(tab) = self.get_mut(id) {
            tab.draft.clear();
        }
    }

    /// The queued texts of one tab, oldest first (the close path reads them before removing it).
    pub fn queue_texts(&self, id: TabId) -> Vec<String> {
        self.get(id)
            .map(|t| t.queue.iter().map(|q| q.text.clone()).collect())
            .unwrap_or_default()
    }

    /// Every tab's queue, summed (the window-close prompt): held messages and those sent into a
    /// running turn that the agent has not been seen to read.
    pub fn queued_count(&self) -> usize {
        self.tabs.iter().map(|t| t.queue.len()).sum()
    }

    /// The window is closing; take every queued text, tab by tab, for the history.
    pub fn take_every_queued_text(&mut self) -> Vec<String> {
        self.tabs
            .iter_mut()
            .flat_map(|t| std::mem::take(&mut t.queue).into_iter().map(|q| q.text))
            .collect()
    }

    /// The window is closing: every tab, for the close path to tear down.
    ///
    /// `take_all` leaves the set without tabs, so `active_tab()` would panic afterwards. Only the
    /// close path calls it, after setting `shutting_down`, and nothing reads the set after that
    /// (`eitri_panel::agent_panel` checks `shutting_down` first on every path that reaches the set).
    pub fn take_all(&mut self) -> Vec<Tab> {
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
        std::mem::take(&mut self.tabs)
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
    /// whose agent resumes is listed whatever the window's Claude backend, in `external_tabs`.
    ///
    /// `conversation_id` is the project's (`agent::conversation_id_for_cwd`): every session of one
    /// project shares it, and a tab whose backend has not been built yet has none of its own.
    pub fn saved_snapshot(&self, conversation_id: &str) -> Option<SavedTabs> {
        if self.closing {
            return None;
        }
        let external: Vec<(TabId, SavedExternalTab)> = self
            .tabs
            .iter()
            .filter(|tab| tab.agent.is_external() && (tab.resumable(self.kind) || tab.failed_resume.is_some()))
            .filter_map(|tab| {
                let session = self.saved_session_id(tab)?;
                Some((
                    tab.id,
                    SavedExternalTab {
                        agent: tab.agent.clone(),
                        conversation_id: conversation_id.to_string(),
                        provider_session_id: session,
                        name: tab.name.clone(),
                        mode: tab.mode,
                    },
                ))
            })
            .collect();
        let mut active_external = external.iter().position(|(id, _)| *id == self.active);
        let waiting_active = self.waiting_restore_active();
        let mut external_tabs: Vec<SavedExternalTab> = external.into_iter().map(|(_, saved)| saved).collect();
        // A restore waiting for the trust question has no tab for its sessions yet; they stay in the
        // file meanwhile, so a session started in another tab does not write over them. A dashboard
        // restore's are left out only while the window has nothing else to save: then nothing is written
        // and the file still holds them (`settle_waiting_restore`).
        let waiting = self.waiting_restore_tabs();
        for granted in waiting.iter().filter(|g| g.planned.agent.is_external()) {
            if active_external.is_none() && waiting_active == Some(granted.planned.provider_session_id.as_str()) {
                active_external = Some(external_tabs.len());
            }
            external_tabs.push(SavedExternalTab {
                agent: granted.planned.agent.clone(),
                conversation_id: conversation_id.to_string(),
                provider_session_id: granted.planned.provider_session_id.clone(),
                name: granted.planned.name.clone(),
                mode: granted.mode,
            });
        }
        if self.kind != BackendKind::Sidecar {
            return Some(SavedTabs {
                external_tabs,
                active_external,
                ..SavedTabs::default()
            });
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
                        conversation_id: conversation_id.to_string(),
                        provider_session_id: session,
                        name: tab.name.clone(),
                        mode: tab.mode,
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
                    conversation_id: conversation_id.to_string(),
                    provider_session_id: granted.planned.provider_session_id.clone(),
                    name: granted.planned.name.clone(),
                    mode: granted.mode,
                },
            ));
        }
        Some(SavedTabs {
            tabs: listed.into_iter().map(|(_, saved)| saved).collect(),
            active,
            external_tabs,
            active_external,
        })
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
        tab.provider_session_id().filter(|id| !id.is_empty())
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
        self.saved_snapshot("")
            .is_some_and(|saved| !saved.tabs.is_empty() || !saved.external_tabs.is_empty())
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
        let empty: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| {
                matches!(tab.backend, TabBackend::NotStarted) && tab.name.is_none() && tab.title.is_none()
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
    /// The returned [`RestoreRun`] is fed the results `collect_starts` reports and says when the last
    /// resume has returned.
    pub fn start_restore<F>(&mut self, from: TabId, go: RestoreGo, mut connect: F) -> RestoreRun
    where
        F: FnMut(&AgentId, &str) -> Result<RestoreConnect, String>,
    {
        let total = go.tabs.len() + go.skipped.len();
        let mut run = RestoreRun::new(
            from,
            self.get(from).map_or(self.default_mode, |t| t.mode),
            self.get(from).map_or(AgentId::Claude, |t| t.agent.clone()),
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
            let ResumeRoute::StartIn(target) = self.route_resume_told(from, &planned.agent, &id, false, false) else {
                continue;
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
            run.order.push(target);
            started.push((id, target));
        }
        let on_screen = go
            .active_session
            .as_ref()
            .and_then(|wanted| started.iter().find(|(id, _)| id == wanted))
            .or_else(|| started.first())
            .map(|(_, tab)| *tab);
        if let Some(tab) = on_screen {
            self.select(tab);
        }
        run
    }

    /// A restore whose start was put off (a trust question answered `Escape`, too large to show, or
    /// refused): its sessions come back as failed tabs holding their resume ids, the first in `from`
    /// and the rest in new tabs, so the saved-tabs file keeps them until the user closes a tab or
    /// starts it over with `r`. Without this the restore's sessions would be in no tab and the next
    /// write of the file would leave them out. A session already open in a tab is not listed twice.
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
        let mut kept: Vec<(String, TabId)> = Vec::new();
        for granted in go.tabs {
            let GrantedTab { planned, mode } = granted;
            let id = planned.provider_session_id.clone();
            if self.tab_with_session(&planned.agent, &id).is_some() {
                continue;
            }
            let ResumeRoute::StartIn(target) = self.route_resume_told(from, &planned.agent, &id, false, false) else {
                continue;
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
        let on_screen = go
            .active_session
            .as_ref()
            .and_then(|wanted| kept.iter().find(|(id, _)| id == wanted))
            .or_else(|| kept.first())
            .map(|(_, tab)| *tab);
        if let Some(tab) = on_screen {
            self.select(tab);
        }
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
                    t.reported_start_failure = false;
                    t.agent = agent;
                    self.give_mode(at, mode);
                }
            }
            None => {
                self.remove(tab);
            }
        }
    }
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

/// The exact `(tool_name, input)` seen by a rule or fast-path answer, kept with its candidate.
/// Match later cards by content when `PermissionRequested` has no tool-use id, and remove the
/// candidate. An empty `toolUseId` maps to `None` in `translate.rs`, so id matching cannot find
/// such cards. Use exact `Value` equality, which ignores object key order, as `HumanApprovals`
/// does. The sets are small and short-lived, so no canonical form is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidateCall {
    tool_name: String,
    input: serde_json::Value,
}

/// A saved prefix rule's own candidate: its display string, and the call its answer
/// saw, for the id-less-card match `CandidateCall`'s own doc describes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RuleCandidate {
    rule: String,
    call: CandidateCall,
}

/// Find the candidate removed by a later `PermissionRequested`: match its tool-use id, or
/// match `(tool_name, input)` if the request has no id. Content matching picks the first id
/// in `BTreeMap` order. Identical concurrent `Bash`/`Write`/`Edit`/`NotebookEdit` calls answered
/// the same way may collide, but their arguments and resulting notes are identical, so the
/// row displays the same result either way.
fn dropped_candidate_id<'a, V>(
    candidates: &'a std::collections::BTreeMap<String, V>,
    tool_use_id: &Option<String>,
    tool_name: &str,
    input: &serde_json::Value,
    call_of: impl Fn(&'a V) -> &'a CandidateCall,
) -> Option<String> {
    if let Some(id) = tool_use_id {
        return candidates.contains_key(id).then(|| id.clone());
    }
    for (id, value) in candidates {
        let call = call_of(value);
        if call.tool_name == tool_name && call.input == *input {
            return Some(id.clone());
        }
    }
    None
}

/// Save tool calls answered by a saved prefix rule as pairs of tool-use id and rule text,
/// such as `Bash(git log *)`, in the tab's `notes` for later snapshots.
///
/// Candidates come only from answers `AgentBackend::answer_what_needs_no_human` actually gave.
/// A delivered `PermissionRequested` removes the matching candidate because a card still needs
/// an answer. Match by id when present, otherwise by tool name and input. `ToolCallCompleted`
/// turns each surviving candidate into a note. Matching a started call against a rule or seeing
/// no card does not prove the rule answered: validation may fail before the gate, or bypass
/// may answer the gate instead.
fn note_rule_answers(
    candidates: &mut std::collections::BTreeMap<String, RuleCandidate>,
    notes: &mut std::collections::BTreeMap<String, String>,
    events: &[AgentDomainEvent],
) -> std::collections::BTreeMap<String, String> {
    let mut fresh = std::collections::BTreeMap::new();
    for event in events {
        match event {
            AgentDomainEvent::PermissionRequested {
                tool_use_id,
                tool_name,
                input,
                ..
            } => {
                if let Some(id) = dropped_candidate_id(candidates, tool_use_id, tool_name, input, |c| &c.call) {
                    candidates.remove(&id);
                }
            }
            AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } => {
                if let Some(candidate) = candidates.remove(tool_use_id) {
                    notes.insert(tool_use_id.clone(), candidate.rule.clone());
                    fresh.insert(tool_use_id.clone(), candidate.rule);
                }
            }
            _ => {}
        }
    }
    fresh
}

/// Track `Write`/`Edit`/`NotebookEdit` calls answered by the acceptEdits fast path.
/// Return their tool-use ids and keep them in `notes.allowed_by_auto` for later snapshots.
///
/// `candidates` contains only answers reported by `AgentBackend::answer_what_needs_no_human`.
/// A later `PermissionRequested` removes a candidate by id, or by matching content when the
/// card has no id, through `dropped_candidate_id`. The CLI delivered a card after the gate.
/// `ToolCallCompleted` turns a surviving candidate into a note.
/// Starting and finishing without a card is insufficient: CLI `validateInput` may fail
/// before a hook runs, a card may omit its tool-use id, or bypass may answer the gate.
fn note_auto_edit_answers(
    candidates: &mut std::collections::BTreeMap<String, CandidateCall>,
    notes: &mut BTreeSet<String>,
    events: &[AgentDomainEvent],
) -> BTreeSet<String> {
    let mut fresh = BTreeSet::new();
    for event in events {
        match event {
            AgentDomainEvent::PermissionRequested {
                tool_use_id,
                tool_name,
                input,
                ..
            } => {
                if let Some(id) = dropped_candidate_id(candidates, tool_use_id, tool_name, input, |c| c) {
                    candidates.remove(&id);
                }
            }
            AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } => {
                if candidates.remove(tool_use_id).is_none() {
                    continue;
                }
                notes.insert(tool_use_id.clone());
                fresh.insert(tool_use_id.clone());
            }
            _ => {}
        }
    }
    fresh
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

/// Returns delivered cards this tab can approve now, in `seq` order. Intersects the attention
/// tray with pending permission ids and excludes requests already answered by the host. Bypass
/// includes every CLI prompt regardless of its reason. Non-live tabs return no cards because
/// they have no session holding pending requests.
///
/// Reads `projection()` once and releases it before touching anything else. Both
/// `BypassPlan::approve` and the recheck in `confirm_bypass` use this function so confirmation
/// approves the same set that the prompt counted.
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
            .filter(|p| cards.contains(&p.permission_id) && !tab.host_answered.contains(&p.permission_id))
            .map(|p| (p.seq, p.permission_id.clone()))
            .collect()
    };
    ordered.sort_by_key(|(seq, _)| *seq);
    ordered.into_iter().map(|(_, id)| id).collect()
}

/// A CLI-prompt candidate whose call completes in this batch becomes its row's note.
/// Keep it in `notes` for later snapshots and return it among this batch's new notes.
fn note_prompt_answers(
    candidates: &mut std::collections::BTreeMap<String, String>,
    notes: &mut std::collections::BTreeMap<String, String>,
    events: &[AgentDomainEvent],
) -> std::collections::BTreeMap<String, String> {
    let mut fresh = std::collections::BTreeMap::new();
    for event in events {
        if let AgentDomainEvent::ToolCallCompleted { tool_use_id, .. } = event {
            if let Some(note) = candidates.remove(tool_use_id) {
                notes.insert(tool_use_id.clone(), note.clone());
                fresh.insert(tool_use_id.clone(), note);
            }
        }
    }
    fresh
}

fn refresh_offers(tab: &mut Tab, project_root: &Path) -> bool {
    let Some(backend) = tab.live() else {
        let had = !tab.rule_offers.is_empty();
        tab.rule_offers.clear();
        tab.rule_offers_seen.clear();
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
            .filter(|p| p.provider_prompt.is_none() && !tab.user_answered.contains(&p.permission_id))
            .collect();
        cards.sort_by(|a, b| a.permission_id.cmp(&b.permission_id));
        // Compared by id before anything is copied: this runs on every tick, and a card's input
        // can be a whole file's content.
        let same = cards.len() == tab.rule_offers_seen.len()
            && cards
                .iter()
                .zip(&tab.rule_offers_seen)
                .all(|(p, seen)| &p.permission_id == seen);
        if same {
            return false;
        }
        cards
            .into_iter()
            .map(|p| (p.permission_id.clone(), p.tool_name.clone(), p.input.clone()))
            .collect()
    };
    tab.rule_offers_seen = pending.iter().map(|p| p.0.clone()).collect();
    let offers: std::collections::BTreeMap<String, agent::PrefixRule> = pending
        .into_iter()
        .filter_map(|(id, tool, input)| agent::permission_rules::offer(&tool, &input, project_root).map(|r| (id, r)))
        .collect();
    let changed = offers != tab.rule_offers;
    tab.rule_offers = offers;
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
/// turn of its own, as Claude Code runs a command typed while it works. Codex lists none, and takes a
/// message into its turn as typed.
fn reads_slash_commands(agent: &AgentId, projection: &agent::AgentSessionProjection) -> bool {
    matches!(agent, AgentId::Claude)
        || projection
            .available_commands
            .as_ref()
            .is_some_and(|list| !list.is_empty())
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
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
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

    /// Create a `Write` request that needs a human: `.git/main.rs` is protected, while an ordinary
    /// in-project write can be allowed by the acceptEdits fast path without a card. The queueing,
    /// attention and confirmation tests need a request that reliably produces a card.
    /// The fast path has separate tests in `agent::permission_policy`.
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
        for mut tab in set.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
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
        set.get_mut(fresh).unwrap().title = Some("its own prompt".into());
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
            set.get(resumed).unwrap().title.as_deref(),
            Some("CLI title of s-resumed")
        );
        assert_eq!(set.get(fresh).unwrap().title.as_deref(), Some("its own prompt"));
        assert!(!set.retitle_from(titles), "the same title again changes nothing");
        assert!(
            !set.retitle_from(|_| Some("retitled again".into())),
            "a later retitle is not followed, as the read at the resume was not"
        );
        assert_eq!(
            set.get(resumed).unwrap().title.as_deref(),
            Some("CLI title of s-resumed")
        );
        assert!(!set.get(resumed).unwrap().title_from_record);
        // A record whose title already was the CLI's (the cache was warm at the resume): kept, and done.
        set.get_mut(resumed).unwrap().title_from_record = true;
        assert!(!set.retitle_from(|_| Some("CLI title of s-resumed".into())));
        assert!(!set.get(resumed).unwrap().title_from_record);
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
        assert_eq!(set.select_number(1), Some(one));
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

    /// The pump calls `take_ui_delivery` on every tab, so policy answers a background tab's
    /// `Read` even while nothing from that tab is dispatched to the panel.
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
        assert!(set.get(background).unwrap().stale);
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
        // The projection holds both requests until the Read's resolution returns.
        // `host_answered` hides the already answered Read from the snapshot and tray, so only
        // the Write that is still waiting is shown.
        assert_eq!(
            snapshot["state"]["pendingPermissions"].as_array().unwrap().len(),
            1,
            "the auto-answered Read is hidden; only the Write is a real card"
        );
        assert_eq!(snapshot["state"]["pendingPermissions"][0]["permissionId"], "perm-write");
        assert!(!set.get(background).unwrap().stale);
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

    /// The snapshot sent by a switch or reload (`active_state_payloads`), followed by the
    /// active payload of each of the next two ticks.
    struct P1a2Run {
        snapshot: String,
        next: Option<String>,
        after: Option<String>,
    }

    /// The ingestion thread folds a turn's text without an intervening tick. A switch or reload
    /// snapshot carries that text while it is still queued. `redeliver` disables the snapshot
    /// coverage filter (`TabSet::redeliver_covered`).
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

    /// A tick delivers the turn's first text. The seam folds `<between>` just after the next
    /// drain, the switch or reload snapshot carries it, and `<after>` folds after that
    /// snapshot. `redeliver` disables the snapshot coverage filter.
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

    /// A switch or reload snapshot carries the text folded mid-stream. Subsequent ticks send
    /// nothing when the snapshot already covered the whole batch, preventing duplicate reply
    /// text in the panel.
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

    /// The drain and snapshot read acquire the lock separately. An event folded between them is
    /// both in the snapshot and still queued, so the next tick must filter it out. The seam
    /// also checks that an event folded after the snapshot reaches the panel exactly once.
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

    /// Disabling the snapshot coverage filter makes both cases repeat the snapshot's text on
    /// the next tick. The reducer fixture replays the actual snapshots and following `events`
    /// payloads with and without filtering through `reducer.ts`.
    ///
    /// The fixture is compared on every run and rewritten only when explicit fixture writing is
    /// enabled and its contents differ. Automatic writes on failure dirty a tracked file, and
    /// changes under `agent-ui/web/src` trigger the npm rebuild in `shell/build.rs`. Checkouts
    /// without the web tree skip the comparison.
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

    /// A `TurnCompleted` carried by a switch or reload snapshot must still reach
    /// `PumpOutput::turn_ended` exactly once when the next tick drains it, even though the
    /// payload filters it out. That signal triggers the panel's queue flush; losing it leaves
    /// the next message waiting. The completion folds without an intervening tick.
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

        set.queue_message(tab, "follow-up", "follow-up-wire".into(), 0, true)
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
            set.get(tab).unwrap().queue.len(),
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

    /// In `pump`'s `Resync` arm, the snapshot is read after further lock acquisitions and a
    /// bypass sweep. A delta folded after the drain is both in that snapshot and still queued,
    /// so the next tick must filter it out. The seam folds a unique delta just after an
    /// overflow drain.
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

    /// The `Resync` payload must carry the start time of the turn in its own `SnapshotView::of`
    /// read. Reading the clock just after the drain can pair an old turn's start time with a
    /// new turn's snapshot if the turn changes during later lock acquisitions and the bypass
    /// sweep. This seam runs immediately before the snapshot read, beyond the reach of
    /// `after_drain`.
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

    /// A snapshot's revision belongs to its session. Replacing the session immediately after a
    /// snapshot must not filter the new session's events against the old snapshot, since
    /// revision numbers restart from the new session's seed. This test installs the replacement
    /// through `collect_starts`.
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

    /// Replacing a backend must discard its snapshot watermark. Fatal-command and never-opened
    /// paths can install `Failed` without `collect_starts` or a pump. This test installs the
    /// next session immediately afterward, with no intervening tick.
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

    /// The shell reports tray attention around ticks and switches, but not panel reloads
    /// (`InboundMessage::Ready`). Reading the active state must leave attention unchanged. The
    /// snapshot draws a newly folded card, and the next pump counts it without sending the
    /// request again.
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
        assert!(!set.get(active).unwrap().stale);
        assert_eq!(set.running_count(), 1, "a turn is running");
        shut_down_all(&mut set);
    }

    /// A connect that finishes after its tab was closed is never
    /// installed anywhere; the closed tab's receiver hands the backend back for shutdown.
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
        let TabBackend::Starting(pending) = closed.backend else {
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

    /// A resumed session with both a rename and a title uses the rename as the tab's label and
    /// `name`, including in the detail popover (`prefix ,`). A rename made while connecting is newer
    /// and takes precedence.
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
        assert_eq!(tab.title.as_deref(), Some("fix the parser"));
        assert_eq!(tab.label_name(), "parser work");

        let second = set.open(AgentId::Claude);
        let tx = starting(&mut set, second, Some("fix the parser"), Some("parser work"));
        set.rename(second, "mine");
        tx.send(Ok(live(&dir).1)).unwrap();
        set.collect_starts();
        assert_eq!(set.get(second).unwrap().name.as_deref(), Some("mine"));
        shut_down_all(&mut set);
    }

    /// `react` responds only when `arrived` increases. The window total must not decrease when
    /// a tab installs a session, resets, or closes, or a card arriving in another tab in the same tick
    /// could go unnoticed.
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
        assert_eq!(set.open_session_ids(), vec!["claude-open".to_string()]);
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

    /// An active Auto tab asks for confirmation before entering bypass both before and after
    /// its session starts. Leaving bypass is immediate in either state.
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

        // Before a session starts in `NotStarted`/auto, entering bypass requires confirmation.
        // Confirming also updates the window default, so a new tab inherits it without another prompt.
        let first_plan = plan(set.cycle_mode(tab));
        assert_eq!(first_plan.scope, BypassScope::Tab(tab));
        assert_eq!(
            set.confirm_bypass(first_plan.scope, first_plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
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
        // Leaving bypass works while `next` is on screen: the tab being changed need not be active.
        assert_eq!(
            set.cycle_mode(tab),
            Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
            "leaving bypass on a live tab moves at once, to manual"
        );
        // Manual -> auto moves at once too, and needs no tab on screen.
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));
        // Entering bypass again still requires confirmation once `tab` is back on screen,
        // just as it did before the session started.
        set.select(tab);
        let second_plan = plan(set.cycle_mode(tab));
        assert_eq!(second_plan.scope, BypassScope::Tab(tab));
        shut_down_all(&mut set);
    }

    #[test]
    fn cycle_default_mode_moves_the_window_default_and_leaves_open_tabs_alone() {
        let mut set = set();
        let tab = set.active();
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto);

        let plan = match set.cycle_default_mode() {
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
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "the open tab's own mode is untouched"
        );
        assert_eq!(
            set.cycle_default_mode(),
            ModeCycle::Changed(SessionModeChoice::Manual),
            "leaving bypass moves at once, to manual"
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Manual);
        assert_eq!(
            set.cycle_default_mode(),
            ModeCycle::Changed(SessionModeChoice::Auto),
            "manual -> auto moves at once"
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert!(
            matches!(set.cycle_default_mode(), ModeCycle::Confirm(_)),
            "and auto -> bypass asks again"
        );
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "the open tab's own mode is untouched throughout"
        );
        shut_down_all(&mut set);
    }

    // Entering bypass asks first; once entered, the host answers every request.

    #[test]
    fn entering_bypass_on_a_live_tab_asks_and_changes_nothing_until_confirmed() {
        let dir = workspace("bypass-ask-first");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let _plan = plan(set.cycle_mode(tab));
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

        // `open()` left `second` active. Entering bypass is refused for a tab that is not on screen,
        // so select `first` before requesting the switch.
        set.select(first);
        let plan = plan(set.cycle_mode(first));
        assert_eq!(plan.approve, vec!["p1".to_string()]);
        assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);

        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 1,
                resolved: vec![]
            })
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
        let old_plan = plan(set.cycle_mode(tab));
        assert_eq!(old_plan.approve, vec!["p1".to_string()]);

        provider.queue(write("p2"));
        until("p2's card too", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });

        let reprompted = match set.confirm_bypass(old_plan.scope, old_plan.nonce) {
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
            set.confirm_bypass(old_plan.scope, old_plan.nonce),
            Err("that prompt is no longer current".to_string()),
            "the OLD nonce is dead now"
        );
        assert_eq!(
            set.confirm_bypass(reprompted.scope, reprompted.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 2,
                resolved: vec![]
            })
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

        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
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
            set.get(tab).unwrap().host_answered,
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
            set.get(tab).unwrap().host_answered.is_empty()
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
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );

        // Leaving bypass lands on manual, where the policy answers again (and an edit cards).
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Manual)));

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
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(plan.scope, BypassScope::Tab(tab));
        assert!(
            plan.lines[0].contains("New sessions in this window start in bypass too"),
            "{:?}",
            plan.lines
        );

        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);

        let next = set.open(AgentId::Claude);
        assert_eq!(
            set.get(next).unwrap().mode,
            SessionModeChoice::Bypass,
            "inherited unasked"
        );
        assert_eq!(
            set.confirm_bypass(BypassScope::Tab(next), 0),
            Err("that prompt is no longer current".to_string()),
            "open() left no stray prompt behind"
        );
        shut_down_all(&mut set);
    }

    #[test]
    fn the_chooser_default_asks_before_bypass_and_not_before_auto() {
        let mut set = set();
        let tab = set.active();
        match set.cycle_default_mode() {
            ModeCycle::Confirm(plan) => {
                assert_eq!(plan.scope, BypassScope::Default);
                assert_eq!(
                    set.confirm_bypass(plan.scope, plan.nonce),
                    Ok(ConfirmOutcome::Entered {
                        approved: 0,
                        resolved: vec![]
                    })
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
            set.cycle_default_mode(),
            ModeCycle::Changed(SessionModeChoice::Manual),
            "bypass -> manual never asks"
        );
        assert_eq!(
            set.cycle_default_mode(),
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
            assert_eq!(set.cycle_mode(ended_auto), Err("the session has ended".to_string()));
            shut_down_all(&mut set);
        }

        // A `Starting` tab asks like a live one: `Confirm`, `LiveTab` text and no cards.
        // After starting fails, the `Failed` bypass tab can still leave bypass immediately.
        {
            let mut set = set();
            let starting_tab = set.active();
            let tx = starting(&mut set, starting_tab, None, None);
            let plan = plan(set.cycle_mode(starting_tab));
            assert_eq!(plan.approve, Vec::<String>::new());
            assert_eq!(
                plan.lines,
                vec!["Switch to bypass? (y/n)".to_string()],
                "a starting tab asks like a live one"
            );
            assert_eq!(
                set.confirm_bypass(plan.scope, plan.nonce),
                Ok(ConfirmOutcome::Entered {
                    approved: 0,
                    resolved: vec![]
                })
            );
            tx.send(Err(BackendError {
                message: "boom".into(),
                benign: false,
                folded_events: Vec::new(),
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
                set.cycle_mode(starting_tab),
                Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
                "a failed bypass tab still leaves at once"
            );
            assert_eq!(
                set.cycle_mode(starting_tab),
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
            let plan = plan(set.cycle_mode(ended_bypass));
            set.confirm_bypass(plan.scope, plan.nonce).unwrap();
            set.get_mut(ended_bypass).unwrap().live_mut().unwrap().shutdown();
            provider.queue(AgentDomainEvent::SessionClosed {
                reason: "closed".into(),
            });
            until("ended", || {
                set.pump(&dir, true);
                set.get(ended_bypass).unwrap().wire_state() == TabStateWire::Ended
            });
            assert_eq!(
                set.cycle_mode(ended_bypass),
                Ok(ModeCycle::Changed(SessionModeChoice::Manual)),
                "an ended bypass tab still leaves at once"
            );
            shut_down_all(&mut set);
        }

        // ...and `reset` on a still-bypass ended tab keeps bypass and leaves the default untouched
        //.
        {
            let mut set = set();
            let ended_bypass = set.active();
            let (provider, backend) = live(&dir);
            set.get_mut(ended_bypass).unwrap().backend = TabBackend::Live(backend);
            let plan = plan(set.cycle_mode(ended_bypass));
            set.confirm_bypass(plan.scope, plan.nonce).unwrap();
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

        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );

        provider.queue(write("pre"));
        until(
            "pre answered, still pending (the double no test provides its own resolution)",
            || {
                set.pump(&dir, true);
                set.get(tab).unwrap().host_answered.contains("pre")
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
            set.get(tab)
                .unwrap()
                .prompt_note_candidates
                .get("toolu_lost_ask")
                .map(String::as_str),
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
            set.get(tab).unwrap().host_answered.contains("seen")
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

    /// `Tab::mode` remains authoritative regardless of a provider's mode report.
    /// A real provider reports an unrestricted mode as `UngatedCliMode`, rather than the raw
    /// `PermissionModeChanged` event used here.
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
        let plan = plan(set.cycle_mode(bypass_tab));
        set.confirm_bypass(plan.scope, plan.nonce).unwrap();
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
            let plan = plan(set.cycle_mode(first));
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
                set.confirm_bypass(plan.scope, plan.nonce),
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
        let first_plan = plan(set.cycle_mode(first));
        set.select(second);
        set.select(first);
        assert_eq!(
            set.confirm_bypass(first_plan.scope, first_plan.nonce),
            Err("that prompt is no longer current".to_string())
        );

        let second_plan = plan(set.cycle_mode(first));
        set.drop_bypass_prompt();
        assert_eq!(
            set.confirm_bypass(second_plan.scope, second_plan.nonce),
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
            set.cycle_mode(second),
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

        // The request stays pending in the projection until its resolution is queued.
        // `RecordingProvider` emits no resolution on its own, so the classifier's answer
        // remains unresolved here.
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

        let plan = plan(set.cycle_mode(tab));
        assert_eq!(plan.approve, vec!["real-card".to_string()]);
        assert!(plan.lines[0].contains("1 waiting card"), "{:?}", plan.lines);
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 1,
                resolved: vec![]
            }),
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

        let plan = plan(set.cycle_mode(tab));
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
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);

        let (_p, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Bypass,
            "starting the session kept the mode"
        );

        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Manual)));
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "leaving bypass on ANY tab drops an in-window default too, to auto"
        );

        let next = set.open(AgentId::Claude);
        assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Auto);
        shut_down_all(&mut set);
    }

    /// A live-tab bypass prompt does not ask about new sessions. If a terminal handoff changes
    /// the tab to `NotStarted` before `y` arrives, ask again without entering bypass or changing
    /// the window default. The saved prompt scope must still describe the tab when answered.
    #[test]
    fn a_live_tab_prompt_answered_after_the_tab_became_empty_reprompts_and_moves_nothing() {
        let dir = workspace("bypass-prompt-kind-changed");
        let mut set = set();
        let tab = set.active();
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let live_plan = plan(set.cycle_mode(tab));
        assert_eq!(live_plan.prompt, tabs::PromptScope::LiveTab);
        assert_eq!(
            live_plan.lines,
            vec![tabs::bypass_prompt(tabs::PromptScope::LiveTab, 0)]
        );

        // What `agent_panel`'s `HandoffToTerminal` arm does to the tab (its own drop of the prompt
        // is the other half of this fix; this is the half that holds even without it).
        if let TabBackend::Live(mut backend) =
            std::mem::replace(&mut set.get_mut(tab).unwrap().backend, TabBackend::NotStarted)
        {
            backend.shutdown();
        }

        let fresh = match set.confirm_bypass(live_plan.scope, live_plan.nonce) {
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
            set.confirm_bypass(live_plan.scope, live_plan.nonce),
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
        let empty_plan = plan(set.cycle_mode(tab));
        assert_eq!(empty_plan.prompt, tabs::PromptScope::EmptyTab);
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);

        let fresh = match set.confirm_bypass(empty_plan.scope, empty_plan.nonce) {
            Ok(ConfirmOutcome::Reprompt(fresh)) => fresh,
            other => panic!("expected a reprompt: {other:?}"),
        };
        assert_eq!(fresh.prompt, tabs::PromptScope::LiveTab);
        assert_eq!(
            set.confirm_bypass(fresh.scope, fresh.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: vec![]
            })
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
        let default_plan = match set.cycle_default_mode() {
            ModeCycle::Confirm(plan) => plan,
            other => panic!("auto -> bypass asks: {other:?}"),
        };
        set.reset(tab).unwrap();
        assert_eq!(
            set.confirm_bypass(default_plan.scope, default_plan.nonce),
            Err("that prompt is no longer current".to_string())
        );
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
    }

    /// A window starts in Auto even if its constructor receives Bypass.
    /// The constructor enforces this independently of `agent_prefs::startup_mode`.
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
        set.mark_seen();
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
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
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
            set.get(tab).unwrap().was_running == running
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
            set.queue_message(tab, "and the tests", "ctx A\n\nand the tests".into(), 1, true),
            Ok(1)
        );
        assert_eq!(
            set.queue_message(tab, "then commit", "ctx B\n\nthen commit".into(), 2, true),
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
        assert!(set.get(tab).unwrap().queue.is_empty());
        shut_down_all(&mut set);
    }

    /// Like `:bd` with unsaved changes, `<leader>bd` asks only when something would be lost.
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
            "a connect in flight"
        );

        let (provider, backend) = live(&dir);
        set.get_mut(empty).unwrap().backend = TabBackend::Live(backend);
        assert!(!set.close_needs_confirm(empty), "an idle live tab: nothing to lose");
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, empty, true);
        assert!(set.close_needs_confirm(empty), "a running turn");
        assert_eq!(set.queue_message(empty, "later", "later".into(), 1, true), Ok(1));
        assert!(set.close_needs_confirm(empty), "a running turn with a queue");
        shut_down_all(&mut set);
    }

    /// The phase-3 GUI pass (2026-09-25): the elapsed clock read `0s+` after a tab switch, because
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
        assert_eq!(set.get(tab).unwrap().turn_times_unsent, 0, "the events carried it");
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
            !set.get(background).unwrap().was_running
        });
        assert_eq!(set.get(background).unwrap().turn_times_unsent, 1);
        set.select(background);
        let times = snapshot_turn_times(&mut set);
        assert_eq!(times[0]["turnId"], "t1");
        assert_eq!(set.get(background).unwrap().turn_times_unsent, 0);
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

    /// A flush the backend refuses stays in the queue with a line saying
    /// why, and is not retried on every tick.
    #[test]
    fn a_refused_flush_keeps_the_queue_and_is_not_retried_every_tick() {
        let dir = workspace("tabs-queue-refused");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        set.queue_message(tab, "later", "later".into(), 1, true).unwrap();
        provider.refuse_sends(true);
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        pump_until_running(&mut set, &dir, tab, false);
        let flush = set.flush_queue(tab).unwrap();
        assert!(flush.outcome.is_err());
        let t = set.get(tab).unwrap();
        assert_eq!(t.queue.len(), 1, "never dropped");
        assert!(t.queue_error.is_some(), "the line saying why");
        for _ in 0..10 {
            assert!(
                set.pump(&dir, true).turn_ended.is_empty(),
                "no new turn end, so no retry"
            );
        }
        assert_eq!(provider.turns().len(), 1, "one attempt");
        shut_down_all(&mut set);
    }

    /// A pending card holds the queue.
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
        set.queue_message(tab, "after the card", "after the card".into(), 1, true)
            .unwrap();
        assert!(set.flush_queue(tab).is_none());
        assert!(provider.turns().is_empty());
        shut_down_all(&mut set);
    }

    /// After an interrupt, the queue still goes out with the message added by `Ctrl+Enter`.
    #[test]
    fn send_now_interrupts_and_the_queue_goes_out_when_the_turn_ends() {
        let dir = workspace("tabs-send-now");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = interruptible_live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        set.queue_message(tab, "one", "one".into(), 1, true).unwrap();
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
        set.queue_message(tab, "a", "a".into(), 1, true).unwrap();
        set.queue_message(tab, "b", "b".into(), 2, true).unwrap();
        assert_eq!(set.take_back_queue(tab), vec!["a".to_string(), "b".to_string()]);
        assert!(set.get(tab).unwrap().queue.is_empty());
        assert!(set.take_back_queue(tab).is_empty());
        shut_down_all(&mut set);
    }

    /// Queueing requires a session that is starting or alive.
    #[test]
    fn queueing_needs_a_session_that_is_starting_or_alive() {
        let mut set = set();
        let tab = set.active();
        assert!(
            set.queue_message(tab, "x", "x".into(), 1, true).is_err(),
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
            set.queue_message(tab, "x", "x".into(), 1, true),
            Ok(1),
            "the box is live while it starts"
        );
        set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "no".into() };
        assert!(set.queue_message(tab, "y", "y".into(), 2, true).is_err());
    }

    /// `r` on an ended tab puts its queue back into the draft.
    #[test]
    fn reset_folds_the_queue_and_the_old_draft_into_the_draft() {
        let mut set = set();
        let tab = set.active();
        {
            let t = set.get_mut(tab).unwrap();
            t.queue.push(Queued::held("q1", "q1".into(), 1));
            t.queue.push(Queued::held("q2", "q2".into(), 2));
            t.draft = "half".into();
            t.backend = TabBackend::Failed { reason: "gone".into() };
        }
        set.reset(tab).unwrap();
        let t = set.get(tab).unwrap();
        assert_eq!(t.draft, "q1\n\nq2\n\nhalf");
        assert!(t.queue.is_empty());
        assert_eq!(set.close_facts(tab).unwrap().queued, 0);
    }

    #[test]
    fn close_facts_and_the_window_count_the_queue_and_a_close_hands_the_texts_back() {
        let mut set = set();
        let one = set.active();
        let two = set.open(AgentId::Claude);
        for (tab, text) in [(one, "a"), (two, "b"), (two, "c")] {
            set.get_mut(tab).unwrap().queue.push(Queued::held(text, text.into(), 0));
        }
        assert_eq!(set.close_facts(two).unwrap().queued, 2);
        assert_eq!(set.queued_count(), 3);
        assert_eq!(
            set.take_every_queued_text(),
            vec!["a".to_string(), "b".into(), "c".into()]
        );
        assert_eq!(set.queued_count(), 0);
    }

    /// Rules answer a background tab's call, and a card that a rule could answer offers that rule.
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
                && set.get(background).unwrap().rule_offers.contains_key("perm-cargo")
        });
        assert_eq!(provider.resolutions(), vec![("perm-npm".to_string(), true)]);
        let offers = &set.get(background).unwrap().rule_offers;
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

    /// An in-project `Write` answered without a card by the acceptEdits fast path is marked
    /// "allowed by auto" in events and subsequent snapshots. A `Write` to a protected path such
    /// as `.git/` keeps its pending card and has no such note.
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

    /// A `Write` allowed by the acceptEdits fast path still marks its completed row as creating
    /// a file when the target was absent. `note_new_files` cannot see this call because
    /// `answer_what_needs_no_human` removes its `PermissionRequested` before delivery.
    /// The absence check runs there just before sending `allow`, so the row does not show an
    /// overwrite warning for a new file.
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
        // Queue the gate request the fast path will answer. The note is recorded at that answer;
        // without a gate, there is nothing to note, as with a call that fails validation first.
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

    /// Queue overflow can drop a follow-up card's `PermissionRequested` while the projection
    /// still shows it pending and `Resync` displays the card. The pending sweep must remove the
    /// fast-path candidate so a later human approval cannot leave an "allowed by auto" note.
    ///
    /// Use an in-project edit the fast path actually answers, then overflow the CLI's follow-up
    /// prompt for that call. A protected `.git/probe` edit would produce a card immediately and
    /// never create the candidate this test needs to remove.
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
            set.get(tab).unwrap().host_answered.contains("perm-gate")
        });
        assert!(set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_edit"));

        // The CLI's own follow-up prompt for the same call (O3), plus filler enough to overflow the
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
            !set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_edit"),
            "the still-pending sweep must drop a candidate whose own further card is genuinely pending"
        );

        set.answer_card(tab, "perm-edit", agent::PermissionDecision::Allow)
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

    /// Builds a tool-call start event for the permission tests below.
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

    /// The CLI runs `validateInput` before `PreToolUse` hooks. An `Edit` of an unread file or one
    /// without `old_string` can finish with `is_error` before any gate exists.
    /// The absence of a card therefore cannot justify an "allowed by auto" note.
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

    /// A request with no tool-use id maps the wire's empty id to `None`.
    /// Its card must still prevent an "allowed by auto" note when a human approves the write.
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
        set.answer_card(tab, "perm-out", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-out"));
        provider.queue(call_done("toolu_out", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_out");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_out").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// Switching to bypass between a call's start and its gate makes bypass answer the request
    /// through `answer_what_needs_no_human`. The completed call must not receive an Auto fast-path note.
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
            set.get(tab).unwrap().host_answered.contains("perm-sw"),
            "bypass answered it"
        );
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_sw").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// Bypass answers a `Write` gate and removes its request before `note_new_files` can see it.
    /// Check whether the target is absent where the answer is given, before sending it, in both
    /// Auto and bypass. A new file gets the creation note; an existing file does not.
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

    /// A created-file note must reach the panel through events as well as snapshots. Answering
    /// and dropping a gate request can leave its batch empty (`RevisedDelivery::Nothing`), so
    /// the note is held for the next events payload instead of being lost with that batch.
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
            set.get(tab).unwrap().host_answered.contains("perm-alone")
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

    /// A fast-path answer stays a candidate until its call completes. If queue overflow drops
    /// the completion event, `Resync` must still turn the finished candidate into its note.
    /// The call finishes before resync here, so the finished sweep consumes the candidate first.
    /// `a_resync_before_the_call_finishes_keeps_its_fast_path_answer` checks the pending sweep's
    /// `host_answered` filter separately.
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
            set.get(tab).unwrap().host_answered.contains("perm-fp")
        });
        assert!(set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_fp"));
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
        assert!(!set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_fp"));
        shut_down_all(&mut set);
    }

    /// A `Resync` can arrive after the fast path answers an edit's gate but before the call ends.
    /// Until the provider reports resolution, the projection still lists the request as pending.
    /// The pending sweep must exclude `host_answered` requests and keep this candidate so the
    /// completed call retains its "allowed by auto" note.
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
            set.get(tab).unwrap().host_answered.contains("perm-pa")
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
            set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_pa"),
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

    /// After the fast path allows a gate, the CLI can raise its own sensitive-file prompt for
    /// the same call. If a human approves that card, completion must not say "allowed by auto".
    /// The `PermissionRequested` arm of `note_auto_edit_answers` removes the candidate.
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
            set.get(tab).unwrap().host_answered.contains("perm-pb-gate")
        });
        assert!(set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_pb"));
        provider.queue(resolved("perm-pb-gate"));
        provider.queue(cli_prompt("perm-pb-cli", "toolu_pb", "Edit", input, None));
        until("the CLI's own prompt as a card", || {
            set.pump(&dir, true)
                .active_payload
                .is_some_and(|p| p.contains("perm-pb-cli"))
        });
        set.answer_card(tab, "perm-pb-cli", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        provider.queue(resolved("perm-pb-cli"));
        provider.queue(call_done("toolu_pb", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_pb");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_pb").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// Only fast-path answers for `Write`, `Edit` and `NotebookEdit` (`ACCEPT_EDITS_TOOLS`) create
    /// Auto candidates. A classifier-allowed `Read` uses the same `answer_what_needs_no_human`
    /// function but must not say "allowed by auto": that note identifies the acceptEdits fast path,
    /// not the read-only rules shared by every mode.
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
            set.get(tab).unwrap().host_answered.contains("perm-pc")
        });
        assert!(!set.get(tab).unwrap().auto_edit_candidates.contains_key("toolu_pc"));
        provider.queue(call_done("toolu_pc", false));
        let payloads = pump_until_done(&mut set, &dir, "toolu_pc");
        assert!(auto_notes_in(&payloads).is_empty(), "{payloads:?}");
        assert!(call_in_snapshot(&mut set, "toolu_pc").get("allowedByAuto").is_none());
        shut_down_all(&mut set);
    }

    /// Queue overflow can drop a follow-up card's `PermissionRequested` while the projection
    /// still shows it pending and `Resync` displays the card. The pending sweep must remove the
    /// rule candidate so a later human approval cannot leave an "allowed by rule" note.
    ///
    /// First let the rule answer the gate to create a real candidate, then overflow the CLI's
    /// follow-up prompt for that call. `ToolCallStarted` alone cannot create a candidate; it must
    /// come from an answer `AgentBackend::answer_what_needs_no_human` actually gave.
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
            set.get(tab).unwrap().rule_candidates.contains_key("toolu_npm")
        });

        // The CLI's own follow-up prompt for the same call (O3), plus filler enough to overflow the
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
            !set.get(tab).unwrap().rule_candidates.contains_key("toolu_npm"),
            "the still-pending sweep must drop a candidate whose own further card is genuinely pending"
        );

        set.answer_card(tab, "perm-npm", agent::PermissionDecision::Allow)
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

    /// When a snapshot covers every event in the next drain, the payload filters them all out.
    /// Notes produced when the tab processes those events still need delivery because the
    /// snapshot predates those notes. An empty `events` envelope carries them to
    /// `applyCallNotes`. The seam folds completion between the drain and the switch's snapshot
    /// read.
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
        // Both targets are under `.git/` so their writes still produce cards. Ordinary top-level
        // writes can be allowed by the acceptEdits fast path without a card, but this test needs
        // cards to check their `createsFile` flags.
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

    /// Only a not-found result marks a path as a new file. An inaccessible parent or a path
    /// through a regular file leaves the overwrite warning in place because the lookup cannot
    /// establish what the write replaces. A dangling symlink also keeps the warning.
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

    /// Seed candidates directly, as `pump` does from `AnsweredForYou::by_rule`.
    /// `note_rule_answers` does not infer them from `ToolCallStarted` arguments. A delivered card
    /// removes a candidate, whether the rule answer failed or the CLI prompted after the gate.
    /// A candidate with no such card becomes a note when its call completes.
    #[test]
    fn note_rule_answers_drops_a_kept_card_and_notes_a_surviving_candidate() {
        let card = |id: &str| AgentDomainEvent::PermissionRequested {
            permission_id: format!("perm-{id}"),
            tool_use_id: Some(id.into()),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "npm ci" }),
            provider_prompt: None,
        };
        let done = |id: &str| AgentDomainEvent::ToolCallCompleted {
            turn_id: "t1".into(),
            tool_use_id: id.into(),
            content: serde_json::json!("ok"),
            is_error: false,
        };
        let rule_call = |command: &str| RuleCandidate {
            rule: "Bash(npm ci *)".to_string(),
            call: CandidateCall {
                tool_name: "Bash".to_string(),
                input: serde_json::json!({ "command": command }),
            },
        };
        let mut candidates = std::collections::BTreeMap::new();
        candidates.insert("toolu_carded".to_string(), rule_call("npm ci"));
        candidates.insert("toolu_ruled".to_string(), rule_call("npm ci"));
        let mut notes = std::collections::BTreeMap::new();
        // Spread over two batches, as a card and its answer are: the candidate outlives the first.
        let first = note_rule_answers(&mut candidates, &mut notes, &[card("toolu_carded")]);
        assert!(first.is_empty());
        let second = note_rule_answers(
            &mut candidates,
            &mut notes,
            &[done("toolu_carded"), done("toolu_ruled")],
        );
        assert_eq!(
            second.into_iter().collect::<Vec<_>>(),
            vec![("toolu_ruled".to_string(), "Bash(npm ci *)".to_string())]
        );
        assert!(candidates.is_empty());
        assert!(!notes.contains_key("toolu_carded"), "a card was delivered for it");
        assert_eq!(notes.get("toolu_ruled"), Some(&"Bash(npm ci *)".to_string()));
    }

    /// The CLI runs `validateInput` before `PreToolUse` hooks. A Bash call can fail with `is_error`
    /// without a `PermissionRequested` or any gate for a saved rule to answer.
    /// Matching its started arguments to a rule and seeing no card cannot justify an
    /// "allowed by rule" note.
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

    /// Switching to bypass between a call's start and its gate makes bypass answer the request.
    /// The bypass branch of `answer_what_needs_no_human` does not classify it against saved rules,
    /// so the completed call must not receive an "allowed by rule" note.
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
            set.get(tab).unwrap().host_answered.contains("perm-sw"),
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

    /// The CLI's follow-up prompt for a call already answered by a rule can have an empty tool-use
    /// id, which `translate.rs` maps to `None`. Match this card to its candidate by content through
    /// `dropped_candidate_id`, so human approval cannot leave an "allowed by rule" note.
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
            set.get(tab).unwrap().rule_candidates.contains_key("toolu_idless")
        });

        // The CLI's follow-up prompt has no tool-use id. `needs_its_own_card()` is false
        // because there is no ask rule or unrecognized origin and a reason is given. In Auto,
        // `HumanApprovals::matches` cannot match an id-less prompt, so this becomes a card
        // instead of being answered by the rule again.
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
            !set.get(tab).unwrap().rule_candidates.contains_key("toolu_idless"),
            "the id-less follow-up card must still drop the candidate, matched by content"
        );

        set.answer_card(tab, "perm-follow-up", agent::PermissionDecision::Allow)
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

    /// A rule-eligible request stays a card if the provider refuses the auto-answer. No
    /// `by_rule` candidate is recorded because candidates are added only on success. With an
    /// empty tool-use id, the content match in `note_rule_answers` (`dropped_candidate_id`)
    /// must find nothing to drop. A later human approval must never be labeled "allowed by
    /// rule".
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
            set.get(tab).unwrap().rule_candidates.is_empty(),
            "the answer failed; no candidate was ever recorded"
        );
        provider.refuse_resolutions(false);
        set.answer_card(tab, "perm-refused", agent::PermissionDecision::Allow)
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

    /// If queue overflow drops a rule-answered call's completion, the `Resync` finished sweep
    /// must turn its candidate into a note. Otherwise the candidate lingers indefinitely and
    /// the completed call never shows which rule answered it.
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
            set.get(tab).unwrap().host_answered.contains("perm-rp")
        });
        assert!(set.get(tab).unwrap().rule_candidates.contains_key("toolu_rp"));
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
        assert!(!set.get(tab).unwrap().rule_candidates.contains_key("toolu_rp"));
        shut_down_all(&mut set);
    }

    /// Item (b) of this task's own review: the still-pending sweep's `host_answered` filter, pinned
    /// for `auto_edit_candidates` by `a_resync_before_the_call_finishes_keeps_its_fast_path_answer`,
    /// had no rule counterpart either. A `Resync` that lands after the rule answered a call's gate
    /// but BEFORE its call completes: the provider has not reported the request resolved, so the
    /// projection still lists it pending; the still-pending sweep must leave it out (`host_answered`)
    /// and keep the candidate, or the finished call would lose its "allowed by rule" note to a
    /// `Resync` that merely came early. Fails, exactly like its auto sibling, if the filter is
    /// removed (verified by mutation in this task).
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
            set.get(tab).unwrap().host_answered.contains("perm-rb")
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
            set.get(tab).unwrap().rule_candidates.contains_key("toolu_rb"),
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

    /// Defect 8 (phase 2's sandbox pass): a background tab's turn trace was never observed.
    #[test]
    fn a_background_tabs_trace_is_observed_marked_and_emitted() {
        let dir = workspace("tabs-bg-trace");
        let mut set = set();
        let background = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(background).unwrap().turn_trace = Some(crate::turn_trace::TurnTrace::started_now());
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
                    .turn_trace
                    .as_ref()
                    .is_some_and(crate::turn_trace::TurnTrace::is_finished)
        });
        let trace = set.get(background).unwrap().turn_trace.as_ref().unwrap();
        assert!(trace.line().contains("first_paint_frame=bg"), "{}", trace.line());
        assert!(
            trace.is_emitted(),
            "the pump emits a background trace: nothing else will"
        );
        shut_down_all(&mut set);
    }

    /// A turn folded entirely before the first tick can reach the active panel wholly through
    /// `active_state_payloads`, with no `events` dispatch. No paint report will arrive, so
    /// `observe_in_snapshot` completes its trace with `first_paint_frame=snapshot`. Emission
    /// must use `!dispatched`: gating on inactivity would leave this active tab's trace
    /// unprinted.
    #[test]
    fn a_turn_folded_entirely_inside_a_snapshot_still_completes_and_prints_its_trace() {
        let dir = workspace("tabs-trace-in-snapshot");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        set.get_mut(tab).unwrap().turn_trace = Some(crate::turn_trace::TurnTrace::started_now());

        // The ingestion thread folds the whole turn independently, without a tick draining it.
        provider.queue(started("t1"));
        provider.queue(text_delta("t1", "hi"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        wait_for_fold(&set, tab, "hi");

        // Reads the snapshot and stores its revision on the backend as the watermark for the
        // next pump's `covered` filter.
        let snapshot = the_snapshot(set.active_state_payloads());
        assert!(snapshot_text(&snapshot).contains("hi"), "{snapshot}");

        // The next pump drains the identical batch the snapshot already carried: `shown` covers
        // all of it, `fresh` is empty, nothing dispatches.
        let out = set.pump(&dir, true);
        assert!(
            out.active_payload.is_none(),
            "the whole batch was already in the snapshot"
        );

        let trace = set.get(tab).unwrap().turn_trace.as_ref().unwrap();
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
        set.queue_message(first, "queued", "queued".into(), 5, true).unwrap();
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

    /// Sending the first message from an empty tab clears its mirrored draft, so state payloads
    /// sent after the connect finishes cannot restore the sent text to the new composer.
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

    /// A draft edited in nvim returns to the tab it came from, after a switch too.
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
        assert_eq!(set.get(first).unwrap().draft, "after");
        assert_eq!(set.get(second).unwrap().draft, "second's own", "never the active tab");
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
        assert_eq!(set.get(second).unwrap().draft, "second's own");
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
        assert_eq!(set.get(changed).unwrap().draft, "new");
        assert_eq!(set.get(identical).unwrap().draft, "same\n");
        assert_eq!(set.get(discarded).unwrap().draft, "kept");
        assert_eq!(set.get(silent).unwrap().draft, "also kept");
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

    /// An `UngatedCliMode` fails the tab with a reason naming the setting and mode, and returns
    /// the backend for shutdown. No events from that batch are delivered, so the queued `Write`
    /// never becomes an approvable card and shutdown denies it.
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
        for mut t in tripped {
            t.backend.shutdown();
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
        for mut t in tripped {
            t.backend.shutdown();
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
        for mut t in tripped {
            t.backend.shutdown();
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
            let AgentBackend::Sidecar(conversation) = &backend else {
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
        for mut t in out.tripped {
            t.backend.shutdown();
        }
        shut_down_all(&mut set);
    }

    /// `Tab::mode` is the only authority: a `PermissionModeChanged` that reached the
    /// pump -- which only one naming `default` can -- moves nothing.
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

    // ---- O3: the CLI's own permission prompts through the tab set ---------------------------------

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

    /// In Auto, a human approves the gate's card with `answer_card`, the gate resolves, and the
    /// CLI asks about the same call under a new permission id. The follow-up request is
    /// accepted and answered `allow` without drawing another card.
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
        set.answer_card(tab, "perm-hook", agent::PermissionDecision::Allow)
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
        assert!(set.get(tab).unwrap().human_allowed.is_empty());

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
        set.answer_card(tab, "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert!(set.get(tab).unwrap().human_allowed.contains("toolu_c"));
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
            set.get(tab).unwrap().human_allowed.is_empty()
        });
        shut_down_all(&mut set);
    }

    /// In bypass, the CLI's own prompt is answered and never drawn.
    #[test]
    fn in_bypass_the_clis_own_prompt_is_answered_and_never_delivered() {
        let dir = workspace("o3-bypass");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: Vec::new()
            })
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

    /// A saved rule cannot silence the CLI's own prompt, so its card never offers "Always allow".
    /// The gate's card for the same command can still offer it.
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
        let offers = &set.get(tab).unwrap().rule_offers;
        assert!(offers.contains_key("perm-gate"), "{offers:?}");
        assert!(!offers.contains_key("perm-cli"), "{offers:?}");
        shut_down_all(&mut set);
    }

    /// Entering bypass counts and approves every waiting card in `seq` order, including CLI
    /// prompts forced by user ask rules and prompts with unknown kinds, missing reasons, descriptions,
    /// paths, or stated reasons. The confirmation has one line because no card stays waiting. A call
    /// whose CLI prompt was approved on entry gets a row note when it completes.
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
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(plan.approve, all);
        assert_eq!(
            plan.lines,
            vec!["Switch to bypass and approve the 6 waiting cards? (y/n)".to_string()]
        );
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 6,
                resolved: Vec::new()
            })
        );
        assert_eq!(
            provider.resolutions(),
            all.iter().map(|id| (id.clone(), true)).collect::<Vec<_>>()
        );
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        assert!(all.iter().all(|id| set.get(tab).unwrap().host_answered.contains(id)));
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

    /// A card arriving while the bypass prompt is open was not counted by that prompt, even if
    /// a user ask rule forced it. Confirmation asks again with the new count, and only that
    /// second `y` approves it.
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
        let first = plan(set.cycle_mode(tab));
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
        let again = match set.confirm_bypass(first.scope, first.nonce) {
            Ok(ConfirmOutcome::Reprompt(again)) => again,
            other => panic!("a card the prompt did not count asks again, got {other:?}"),
        };
        assert!(provider.resolutions().is_empty(), "{:?}", provider.resolutions());
        assert_eq!(again.approve, vec!["perm-write".to_string(), "perm-ask".to_string()]);
        assert_eq!(
            set.confirm_bypass(again.scope, again.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 2,
                resolved: Vec::new()
            })
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
            .answer_card(tab, "perm-x", agent::PermissionDecision::Allow)
            .expect_err("no session");
        assert_eq!(error.message, "no active session");
        assert!(error.benign);
        assert!(set
            .answer_card(TabId(999), "perm-x", agent::PermissionDecision::Allow)
            .is_err());
    }

    /// Codex's finding: a `Resync` can swallow a turn boundary (T1's `TurnCompleted` and T2's
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
        set.answer_card(tab, "perm-hook", agent::PermissionDecision::Allow)
            .map_err(|e| e.message)
            .unwrap();
        assert!(set.get(tab).unwrap().human_allowed.contains("toolu_reused"));

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
        assert!(set.get(tab).unwrap().human_allowed.is_empty());

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

    /// `r` forgets the approvals at once, before any pump (review #4, C2).
    #[test]
    fn reset_forgets_the_approvals() {
        let mut set = set();
        let tab = set.active();
        let t = set.get_mut(tab).unwrap();
        t.backend = TabBackend::Failed { reason: "gone".into() };
        t.human_allowed.record("toolu_x", "Write", &serde_json::json!({}));
        set.reset(tab).unwrap();
        assert!(set.get(tab).unwrap().human_allowed.is_empty());
    }

    /// A tab with no backend holds no approvals: the pump's non-live arm forgets them (review #4, C3).
    #[test]
    fn a_tab_without_a_backend_holds_no_approvals() {
        let dir = workspace("o3-no-backend");
        let mut set = set();
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .human_allowed
            .record("toolu_x", "Write", &serde_json::json!({}));
        set.pump(&dir, true);
        assert!(set.get(tab).unwrap().human_allowed.is_empty());
    }

    /// An Approve the provider refused approved nothing, so nothing is recorded (review #4, C4).
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
            .answer_card(tab, "perm-hook", agent::PermissionDecision::Allow)
            .is_err());
        assert!(set.get(tab).unwrap().human_allowed.is_empty());
        shut_down_all(&mut set);
    }

    /// A call whose CLI prompt was answered without a card gets a muted note on its row. The
    /// note is carried in the completion's events payload and every later snapshot, as with a
    /// call answered by a saved rule.
    #[test]
    fn a_call_answered_without_a_card_says_so_on_its_row() {
        let dir = workspace("o3-row-note");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 0,
                resolved: Vec::new()
            })
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
            folded_events: Vec::new(),
        }
    }

    fn ids(saved: &SavedTabs) -> Vec<&str> {
        saved.tabs.iter().map(|t| t.provider_session_id.as_str()).collect()
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
        assert_eq!(saved.active, 1, "the tab on screen, among the listed ones");

        set.select(empty);
        assert_eq!(
            set.saved_snapshot(PROJECT).unwrap().active,
            1,
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
            1,
            "b was the last listed tab on screen"
        );
        set.select(a);
        set.select(c);
        assert_eq!(set.saved_snapshot(PROJECT).unwrap().active, 0, "now it was a");
        // Closing the tab it fell back to hands the fallback to the one before it.
        set.remove(a);
        assert_eq!(set.saved_snapshot(PROJECT).unwrap().active, 0);
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
        for mut tab in taken {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
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
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Manual)),);
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
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "an empty tab carries the default"
        );
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(
            set.get(tab).unwrap().mode(),
            SessionModeChoice::Auto,
            "nothing moves before y"
        );
        set.confirm_bypass(plan.scope, plan.nonce).unwrap();
        assert_eq!(set.get(tab).unwrap().mode(), SessionModeChoice::Bypass);
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Manual)));
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
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));
        let plan = plan(set.cycle_mode(tab));
        // Something other than the key moved the tab under the open prompt.
        set.get_mut(tab).unwrap().mode = SessionModeChoice::Manual;
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
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
        let plan = plan(set.cycle_mode(acp));
        set.confirm_bypass(plan.scope, plan.nonce).unwrap();
        assert_eq!(set.cycle_mode(acp), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Manual,
            "leaving bypass leaves the window default in manual; the tab's auto is only its agent's stand-in"
        );
        assert!(
            matches!(set.cycle_mode(acp), Ok(ModeCycle::Confirm(_))),
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
        assert_eq!(set.cycle_default_mode(), ModeCycle::Changed(SessionModeChoice::Auto));
        let ModeCycle::Confirm(plan) = set.cycle_default_mode() else {
            panic!("auto -> bypass asks")
        };
        set.confirm_bypass(plan.scope, plan.nonce).unwrap();
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(set.cycle_default_mode(), ModeCycle::Changed(SessionModeChoice::Manual));
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
        assert_eq!(on_disk(&state).active, 1);

        set.select(first);
        assert!(show(&set, &mut memory), "the active tab switched");
        assert_eq!(on_disk(&state).active, 0);

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
        for mut tab in taken {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
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
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Sidecar(Box::new(conversation)));
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

        // A legacy turn begins in a send, with no event for the pump to fold: the projection just
        // has an active turn by the next pump.
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
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
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
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
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
        assert!(set.get(tab).unwrap().queue.is_empty());
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
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
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
        if let TabBackend::Live(backend) = &mut closed.backend {
            backend.shutdown();
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
        for mut tab in tabs {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
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
        assert!(set.check_trust_answer(tab, first).is_err());
        assert!(set.mark_recording(tab, first).is_err());
        assert!(set.take_answered(tab, first).is_err());
        assert!(set.cancel_trust(tab, first).is_err());
        assert!(
            awaiting(&set, tab).asking == Some(second),
            "refusals leave the question as it was"
        );
        assert!(set.check_trust_answer(tab, second).is_ok());
        assert!(set.check_trust_answer(tab, 0).is_err());
    }

    #[test]
    fn an_answer_for_a_tab_not_on_screen_is_refused() {
        let (mut set, first, _) = asked(first_send("r1", "hi"));
        let other = set.open(AgentId::Claude);
        assert_eq!(set.active(), other);
        let nonce = set.ask_trust(first).unwrap();
        let why = set.check_trust_answer(first, nonce).unwrap_err();
        assert!(why.contains("not the one on screen"), "{why}");
        assert!(set.mark_recording(first, nonce).is_err());
        assert!(set.take_answered(first, nonce).is_err());
        assert!(set.cancel_trust(first, nonce).is_err());
        assert!(matches!(set.get(first).unwrap().backend, TabBackend::AwaitingTrust(_)));
        assert!(
            set.check_trust_answer(other, nonce).is_err(),
            "and not for a tab that waits for nothing"
        );
        set.select(first);
        let nonce = set.ask_trust(first).unwrap();
        assert!(
            set.check_trust_answer(first, nonce).is_ok(),
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
                    let _ = set.cycle_mode(tab);
                }
                _ => {
                    let _ = set.cycle_default_mode();
                }
            }
            assert!(
                set.check_trust_answer(tab, nonce).is_err(),
                "{how}: the old nonce is refused"
            );
            assert!(set.take_answered(tab, nonce).is_err(), "{how}");
            let waiting = awaiting(&set, tab);
            assert_eq!(waiting.asking, None, "{how}");
            assert!(!waiting.recording, "{how}");
            let fresh = set.ask_trust(tab).unwrap();
            assert!(
                set.check_trust_answer(tab, fresh).is_ok(),
                "{how}: showing it again asks again"
            );
        }
    }

    #[test]
    fn check_trust_answer_consumes_nothing() {
        let (set, tab, nonce) = asked(first_send("r1", "hi"));
        assert!(set.check_trust_answer(tab, nonce + 1).is_err());
        assert!(set.check_trust_answer(tab, nonce).is_ok());
        assert!(set.check_trust_answer(tab, nonce).is_ok(), "a second look still passes");
        let waiting = awaiting(&set, tab);
        assert_eq!(waiting.asking, Some(nonce));
        assert!(!waiting.recording);
        assert!(matches!(waiting.start, DeferredStart::FirstSend { .. }));
    }

    #[test]
    fn an_accepted_answer_hands_the_start_over_once() {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        let start = set.take_answered(tab, nonce).unwrap();
        assert_eq!(start.request_id(), Some("r1"));
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert!(set.take_answered(tab, nonce).is_err(), "the answer is spent");

        let (mut set, tab, nonce) = asked(resume_of("r2", "sess-9"));
        let start = set.cancel_trust(tab, nonce).unwrap();
        assert!(
            matches!(start, DeferredStart::Resume { ref provider_session_id, .. } if provider_session_id == "sess-9")
        );
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
        assert!(set.awaiting_trust().is_empty());
    }

    #[test]
    fn a_recording_tab_waits_for_its_record() {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        set.mark_recording(tab, nonce).unwrap();
        let waiting = awaiting(&set, tab);
        assert!(waiting.recording);
        assert_eq!(waiting.asking, None);
        assert!(set.check_trust_answer(tab, nonce).is_err(), "the nonce is spent");
        assert!(set.take_answered(tab, nonce).is_err());
        assert!(set.cancel_trust(tab, nonce).is_err());
        assert!(
            set.mark_recording(tab, nonce).is_err(),
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
        set.mark_recording(tab, nonce).unwrap();
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
        set.get_mut(tab).unwrap().draft = "and then the tests".into();
        let old = set.reset(tab).expect("a waiting tab starts over");
        assert!(old.is_none(), "nothing was spawned");
        let t = set.get(tab).unwrap();
        assert!(matches!(t.backend, TabBackend::NotStarted));
        assert_eq!(
            t.draft, "fix the build\n\nand then the tests",
            "the typed text comes back first"
        );
        assert_eq!(t.project_tiers, None);
        assert!(set.awaiting_trust().is_empty());

        // A resume has no text to give back: it is dropped.
        let (mut set, tab, _) = asked(resume_of("r2", "sess-1"));
        assert!(set.reset(tab).unwrap().is_none());
        assert!(set.get(tab).unwrap().draft.is_empty());
        assert!(matches!(set.get(tab).unwrap().backend, TabBackend::NotStarted));
    }

    #[test]
    fn closing_an_awaiting_tab_drops_its_start() {
        let (mut set, tab, nonce) = asked(first_send("r1", "hi"));
        let other = set.open(AgentId::Claude);
        let closed = set.remove(tab).expect("the tab closes");
        match closed.backend {
            TabBackend::AwaitingTrust(waiting) => assert_eq!(waiting.start.request_id(), Some("r1")),
            _ => panic!("the tab was waiting"),
        }
        assert!(set.get(tab).is_none());
        assert!(set.awaiting_trust().is_empty());
        assert!(
            set.check_trust_answer(tab, nonce).is_err(),
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
        assert!(set.open_session_ids().is_empty(), "no lease or session is named yet");
        let nonce = set.ask_trust(from).unwrap();
        let DeferredStart::Restore { go } = set.take_answered(from, nonce).unwrap() else {
            panic!("the restore comes back")
        };
        assert_eq!(
            go.session_ids(),
            ["a", "b"],
            "intact, ready to start when the answer allows it"
        );
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
        TurnPolicy { reviewer: r }
    }

    fn background_count(set: &TabSet, tab: TabId) -> u32 {
        set.get(tab).unwrap().live().unwrap().projection().background_calls
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
        let plan = match set.cycle_mode(tab) {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected a question, got {other:?}"),
        };
        set.confirm_bypass(plan.scope, plan.nonce).unwrap();
        send(&mut set, tab, "second");
        assert_eq!(
            provider.policies(),
            vec![reviewer(Reviewer::Agent), reviewer(Reviewer::User)],
            "Auto's turn is reviewed by the agent, Bypass's reaches Eitri"
        );
        for mut t in set.take_all() {
            if let TabBackend::Live(b) = &mut t.backend {
                b.shutdown();
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
        let plan = match set.cycle_mode(tab) {
            Ok(ModeCycle::Confirm(plan)) => plan,
            other => panic!("expected a question, got {other:?}"),
        };
        set.confirm_bypass(plan.scope, plan.nonce).unwrap();
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
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Manual)));
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
        for mut t in set.take_all() {
            if let TabBackend::Live(b) = &mut t.backend {
                b.shutdown();
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
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));
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
        for mut t in set.take_all() {
            if let TabBackend::Live(b) = &mut t.backend {
                b.shutdown();
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
        for mut t in set.take_all() {
            if let TabBackend::Live(b) = &mut t.backend {
                b.shutdown();
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
        set.mark_recording(from, nonce).unwrap();
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
        assert_eq!(saved.external_tabs.len(), 1, "kept in the saved file while it waits");
        assert_eq!(saved.external_tabs[0].provider_session_id, "codex-b");
        assert_eq!(saved.active_external, Some(0));
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
        assert!(saved.tabs.is_empty(), "the empty Claude tab has nothing to save");
        assert_eq!(
            saved.external_tabs,
            vec![crate::saved_tabs::SavedExternalTab {
                agent: AgentId::Codex,
                conversation_id: "conv".into(),
                provider_session_id: "thread-1".into(),
                name: None,
                mode: SessionModeChoice::Bypass,
            }]
        );
        assert_eq!(saved.active_external, Some(0));
        // A window on the legacy Claude backend still keeps its Codex tabs.
        set.kind = BackendKind::Legacy;
        assert_eq!(set.saved_snapshot("conv").unwrap().external_tabs.len(), 1);
        drop(provider);
        for mut t in set.take_all() {
            if let TabBackend::Live(b) = &mut t.backend {
                b.shutdown();
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
            folded_events: Vec::new(),
        }))
        .unwrap();
        let collected = set.collect_starts();
        assert!(matches!(collected.as_slice(), [StartCollected::Failed { tab, .. }] if *tab == codex));
        assert_eq!(set.get(codex).unwrap().wire_state(), TabStateWire::Failed);

        let saved = set.saved_snapshot("conv").unwrap();
        assert_eq!(saved.external_tabs.len(), 1, "the conversation is still in the file");
        assert_eq!(saved.external_tabs[0].agent, AgentId::Codex);
        assert_eq!(saved.external_tabs[0].provider_session_id, "codex-b");
        assert_eq!(saved.external_tabs[0].name.as_deref(), Some("parser work"));
        assert_eq!(saved.external_tabs[0].mode, SessionModeChoice::Bypass);
        assert_eq!(saved.active_external, Some(0));

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
        assert_eq!(set.saved_snapshot("conv").unwrap().external_tabs.len(), 1);
        set.remove(other).unwrap();

        // Starting it over is the user's choice of a fresh session; closing it is the user's choice to let go.
        let mut again = set.saved_snapshot("conv").unwrap();
        assert_eq!(again.external_tabs.len(), 1);
        set.reset(codex).unwrap();
        again = set.saved_snapshot("conv").unwrap();
        assert!(again.external_tabs.is_empty(), "a tab started over holds no session");

        set.get_mut(codex).unwrap().backend = TabBackend::Failed { reason: "x".into() };
        set.get_mut(codex).unwrap().failed_resume = Some("codex-b".into());
        assert_eq!(set.saved_snapshot("conv").unwrap().external_tabs.len(), 1);
        // A second failed tab of the same session (its resume by hand refused too) is not a second entry.
        let twin = set.open(AgentId::Codex);
        set.get_mut(twin).unwrap().backend = TabBackend::Failed { reason: "y".into() };
        set.get_mut(twin).unwrap().failed_resume = Some("codex-b".into());
        assert_eq!(set.saved_snapshot("conv").unwrap().external_tabs.len(), 1);
        set.remove(twin).unwrap();
        set.remove(codex).unwrap();
        assert!(set.saved_snapshot("conv").unwrap().external_tabs.is_empty());
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
            assert_eq!(saved.external_tabs.len(), saved_count, "loadSession {resumes}");
            if resumes {
                assert_eq!(saved.external_tabs[0].agent, agent);
                assert_eq!(saved.external_tabs[0].provider_session_id, "thread-1");
                assert_eq!(saved.active_external, Some(0));
            } else {
                assert_eq!(saved.active_external, None);
            }
            drop(provider);
            for mut t in set.take_all() {
                if let TabBackend::Live(b) = &mut t.backend {
                    b.shutdown();
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
        assert_eq!(saved.external_tabs.len(), 1, "kept in the saved file while it waits");
        assert_eq!(saved.external_tabs[0].provider_session_id, "gem-s");
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
        if let TabBackend::Live(b) = &mut removed.backend {
            b.shutdown();
        }
        set.retire_done(tab);
        assert_eq!(guard.check(), Ok(()));
        // A second report for the same tab changes nothing.
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
            set.get(tab).unwrap().was_running == running
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
        set.queue_message(tab, text, format!("ctx\n\n{text}"), 1, true).unwrap();
        set.steer_held(tab, 2, Some("turn-1"));
    }

    /// The queue as `(typed text, sent into the turn)`.
    fn queue(set: &TabSet, tab: TabId) -> Vec<(String, bool)> {
        set.get(tab)
            .unwrap()
            .queue
            .iter()
            .map(|q| (q.text.clone(), !q.is_held()))
            .collect()
    }

    fn note(set: &TabSet, tab: TabId) -> Option<String> {
        set.get(tab).unwrap().queue_note.clone()
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
        for mut tab in set.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
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
            set.get(tab).unwrap().queue.is_empty()
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
        assert!(set.get(tab).unwrap().queue.is_empty());
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
        assert!(set.get(tab).unwrap().queue.is_empty());
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
            set.get(tab).unwrap().queue.is_empty()
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
        until("the follow-up turn is folded", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .active_turn_id
                .as_deref()
                == Some("turn-2")
        });
        assert_eq!(queue(&set, tab), [("and the tests".to_string(), false)], "held again");
        assert!(set.flush_queue(tab).is_none(), "the follow-up runs");

        set.queue_message(tab, "and the docs", "ctx\n\nand the docs".into(), 3, true)
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
            set.get(tab).unwrap().queue.len() == 1
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
        assert!(!settle_against(&mut queue, &mut note, &projection));
        assert_eq!(queue, [steered.clone()], "the newer copy is still open");

        let mut dropped = projection.clone();
        dropped.apply(&AgentDomainEvent::UserMessageDropped {
            client_message_id: "new".into(),
            reason: "Claude discarded it".into(),
        });
        assert!(settle_against(&mut queue, &mut note, &dropped));
        assert!(queue.is_empty(), "the copy the agent read is enough");
        assert_eq!(note, None, "not held again");

        let mut ended = projection.clone();
        ended.apply(&turn_completed("turn-2", agent::TurnOutcome::Completed));
        let mut queue = vec![steered.clone()];
        assert!(settle_against(&mut queue, &mut note, &ended));
        assert!(queue.is_empty(), "no turn runs, and the agent read it");

        let mut read_twice = projection;
        read_twice.apply(&delivered("new"));
        let mut queue = vec![steered];
        assert!(settle_against(&mut queue, &mut note, &read_twice));
        assert!(queue.is_empty());
    }

    /// A slash command typed while the turn runs waits for the turn's end on an agent that reads commands
    /// (Claude, or one that listed its commands), and so does what follows it; on Codex, which lists none,
    /// it goes into the turn as typed.
    #[test]
    fn a_slash_command_waits_for_the_turns_end_where_the_agent_reads_commands() {
        const WAITS: &str =
            "held until the turn ends: a command waits for the turn's end, and what follows it waits too";

        // An agent that listed its commands.
        let (dir, mut set, tab, provider) = running_codex("steer-command-listed", true);
        provider.inner.queue(AgentDomainEvent::CommandsAvailable {
            commands: vec![agent::SlashCommand {
                name: "compact".into(),
                description: String::new(),
                hint: None,
            }],
        });
        until("the list of commands is folded", || {
            set.pump(&dir, true);
            set.get(tab)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .available_commands
                .is_some()
        });
        set.queue_message(tab, "/compact", "/compact".into(), 1, true).unwrap();
        set.queue_message(tab, "then this", "then this".into(), 1, true)
            .unwrap();
        assert!(set.steer_held(tab, 2, Some("turn-1")), "the note changed");
        assert!(provider.steers().is_empty(), "nothing went into the turn");
        assert_eq!(
            queue(&set, tab),
            [("/compact".to_string(), false), ("then this".to_string(), false)]
        );
        assert_eq!(note(&set, tab).as_deref(), Some(WAITS));
        shut_down(&mut set);

        // Claude reads commands whatever it listed (its tabs cannot steer yet, so this is the rule alone);
        // an ACP agent that withdrew its list (an empty one) reads none.
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

    #[test]
    fn a_refusal_holds_the_message_and_every_one_after_it_keeping_their_order() {
        let (dir, mut set, tab, provider) = running_codex("steer-refused", true);
        provider.refuse_steers(Some(ProviderErrorCode::NoActiveTurn));
        set.queue_message(tab, "first", "first".into(), 1, true).unwrap();
        set.queue_message(tab, "second", "second".into(), 1, true).unwrap();
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
        set.get_mut(tab).unwrap().backend = TabBackend::Live(AgentBackend::Sidecar(Box::new(conversation)));
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
        set.queue_message(tab, "x", "x".into(), 1, true).unwrap();
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
        assert!(set.get(tab).unwrap().queue.is_empty(), "settled without a pump");
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
        assert!(set.get(tab).unwrap().queue.is_empty());
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
        assert!(set.get(tab).unwrap().queue.is_empty());
        assert!(provider.inner.turns().is_empty());
        shut_down(&mut set);
    }

    /// Only into the turn the page showed: one typed in a turn that has ended since, or with none showing,
    /// is held.
    #[test]
    fn a_message_goes_only_into_the_turn_the_page_showed() {
        let (_dir, mut set, tab, provider) = running_codex("steer-shown-turn", true);
        set.queue_message(tab, "for the old turn", "for the old turn".into(), 1, true)
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
                let mut backend = tripped.backend;
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
}
