//! One window's session tabs (session tabs spec §3.1): one conversation, and so one `AgentBackend`,
//! per tab, every one of them running. GTK-free: `shell::agent_panel` spawns the connect and
//! handoff workers and hands this set their receivers; the 33 ms tick calls [`TabSet::pump`], which
//! drains EVERY tab -- `take_ui_delivery` is where the permission policy answers what needs no
//! human, so a tab that was not pumped would stall on its first `Read` (spec §3.1).
//!
//! **Lock order, inherited:** on the sidecar path `projection()` holds the ingestion mutex and
//! `provider_session_id()` takes it again. Never call the second while holding the first's guard
//! (the 2026-09-15 GTK freeze). Every method below reads them in separate statements.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc;

use agent::{AgentDomainEvent, RevisedDelivery};

use crate::agent_backend::{AgentBackend, BackendError, BackendKind};
use crate::agent_bridge::{
    serialize_events_with_notes_for_js, serialize_handoff_for_js, serialize_snapshot_with_notes_for_js,
    serialize_tabs_for_js, CallNotes, SessionModeChoice, SnapshotView, TabRef, TabStateWire, TabView,
};
use crate::attention::{Attention, AttentionTracker};
use crate::saved_tabs::{SavedTab, SavedTabs};
use crate::tab_restore::{PlannedTab, RestorePlan, RestoreRun, Skipped};
use crate::tabs::{self, TabFacts, TabId};

/// The first turn of a session started by it (ruling 4), composed when Enter was pressed.
pub struct FirstTurn {
    /// With the editor context composed in: what goes to the model.
    pub wire: String,
    /// As typed: what the panel shows and what titles the session.
    pub typed: String,
}

/// A backend being constructed on a worker. See `shell::agent_panel`'s connect worker.
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
    /// That session's rename (`ConversationRecord.name`), which the label puts first (spec §3.2).
    /// Installed as the tab's own `name`, so the detail popover and `prefix ,` see it too, unless
    /// the tab was renamed while it connected.
    pub resumed_name: Option<String>,
}

/// A handoff whose session close is still running on a worker thread.
///
/// The command is built BEFORE the session is taken -- it is derived from the session's own state,
/// which stops being readable the moment ownership moves to the worker -- and dispatched only after
/// `closed_rx` reports. That ordering is the whole reason this struct exists rather than a straight
/// reply: design doc §8.3 requires the session to be closed and flushed before the CLI starts, and
/// the user cannot start it before they have seen the command.
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

/// One message typed while a turn ran (D4). `wire` is composed against the editor context of the
/// moment it was queued (spec §4.1); `text` is what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub text: String,
    pub wire: String,
    pub queued_at_ms: u64,
}

/// What a flush sent, and how the backend answered (ruling 2).
pub struct Flush {
    pub typed: String,
    pub outcome: Result<Vec<agent::AgentDomainEvent>, BackendError>,
}

pub enum SendNow {
    /// A turn was running: it was asked to stop; the queue goes out when it ends (ruling 8).
    Interrupting(Result<Vec<agent::AgentDomainEvent>, BackendError>),
    /// Idle: flushed at once (`None` when there was nothing to send).
    Flushed(Option<Flush>),
}

/// Not boxed: a window holds a handful of tabs, `Live` is the state a tab spends its life in, and
/// the plan's interface (Task 6) matches on `Live(AgentBackend)` directly.
#[allow(clippy::large_enum_variant)]
pub enum TabBackend {
    /// An empty tab: a live composer and no process (spec §3.6). A session is created on the first
    /// send or resume.
    NotStarted,
    Starting(PendingStart),
    /// A backend, whether its session is running or has ended (the projection says which).
    Live(AgentBackend),
    /// Never worked: a start that failed, a session that never opened, or a fatal command (ruling 14).
    Failed {
        reason: String,
    },
}

pub struct Tab {
    pub id: TabId,
    pub number: u16,
    /// The rename (`prefix ,`), already normalized.
    pub name: Option<String>,
    /// The first prompt's title, or the resumed record's display title.
    pub title: Option<String>,
    /// Was React's `startedPermissionMode`; now survives a panel reload (spec §3.8 point 3).
    /// Private outside this module for the same reason `host_answered` is (R06/S2): S2's rule that
    /// entering bypass always asks first only holds if nothing but `cycle_mode`/`confirm_bypass` can
    /// ever move it there. `mode()` below is the read-only way out; this module's own tests still
    /// write the field directly.
    mode: SessionModeChoice,
    pub backend: TabBackend,
    pub attention: AttentionTracker,
    /// The WebView's copy is behind: this tab took events while it was not active (ruling 18).
    pub stale: bool,
    pub turn_trace: Option<crate::turn_trace::TurnTrace>,
    pub reported_start_failure: bool,
    pub pending_handoff: Option<PendingHandoff>,
    /// Held here, not only in the WebView, so a panel reload cannot lose it (see the doc this field
    /// had on `AgentPanelState` before session tabs; it moved per tab with ruling 13).
    pub last_handoff: Option<agent::handoff::ClaudeResumeCommand>,
    /// Messages typed while a turn ran, oldest first (D4, ruling 1). Flushed as one turn when the
    /// turn ends (ruling 3), never while a card waits (ruling 4).
    pub queue: Vec<Queued>,
    /// Why the last flush was refused (spec §4.4). Cleared by a flush that goes out, a take-back or
    /// a reset.
    pub queue_error: Option<String>,
    /// The panel's composer text, mirrored here so a switch or a reload cannot lose it (ruling 6).
    pub draft: String,
    /// Permission id -> the rule the card offers (D7, ruling 16): only for a card a rule would answer.
    pub rule_offers: std::collections::BTreeMap<String, agent::PrefixRule>,
    /// The scratch edit of the draft in nvim, when one is open (set by the shell's round trip).
    pub editing_draft: Option<u64>,
    /// Whether a turn is running in what the pump has DELIVERED (a `TurnStarted` without its
    /// `TurnCompleted`), so it reports a turn's end exactly once (ruling 3a). Read off the event
    /// stream rather than re-read from `projection()` after the delivery: on the sidecar path the
    /// ingestion thread folds into the projection concurrently, so a re-read can already include a
    /// completion the delivery did not carry, and the end was then never reported.
    was_running: bool,
    /// What the rows and cards say beyond the projection, kept for every later snapshot: the saved
    /// rule that answered a call (v1 polish F18, `note_rule_answers`), and the `Write` cards whose
    /// file did not exist when they were raised (F22, `note_new_files`).
    notes: CallNotes,
    /// Tool-use id -> the rule (and the `(tool_name, input)` its answer saw) whose gate
    /// `AgentBackend::answer_what_needs_no_human` answered, until the call completes; it then
    /// becomes `notes.allowed_by_rule`, exactly as `auto_edit_candidates` does (v1 trial
    /// whole-branch review, fix round 3 -- the same fix as finding 2, moved to this pre-existing F18
    /// note). Filled only from what that function reports it answered (`AnsweredForYou`'s
    /// `by_rule`) -- never from a call that merely started and a rule WOULD answer, re-derived from
    /// that `ToolCallStarted`'s own arguments, which was also true of a call that failed the CLI's
    /// own validation before any gate, and of a gate a switch to bypass answered instead of the
    /// rule. A card naming this id later kept for the same call drops it (the CLI's own prompt after
    /// the gate); so does a later card naming NO id at all, whose own `(tool_name, input)` matches a
    /// candidate's (review item 2: Verdandi's `permissionBroker.ts` sends `toolUseId: ''` when the
    /// CLI gave none, and `translate.rs` maps that to `None`, so such a card can never be found by
    /// id at all -- without this it would still read "allowed by rule" once a human approved it and
    /// the call completed). A `Resync` turns one whose call finished among the dropped events into
    /// its note. Cleared when a fresh backend is installed (`collect_starts`); on a `Resync`, an
    /// entry whose id is still among the projection's pending permissions that nobody answered for
    /// the user is dropped rather than kept -- the removing `PermissionRequested` a queue overflow
    /// can drop, the same sibling gap `auto_edit_candidates` has.
    rule_candidates: std::collections::BTreeMap<String, RuleCandidate>,
    /// Tool-use id -> the `(tool_name, input)` its answer saw, for a `Write`/`Edit`/`NotebookEdit`
    /// whose gate the acceptEdits fast path answered (v1 trial item 7), until the call completes; it
    /// then becomes `notes.allowed_by_auto`, as a prompt-note candidate does
    /// (`note_auto_edit_answers`). Filled only from what `AgentBackend::take_revised_ui_delivery`
    /// reports it answered (`AnsweredForYou`'s `by_the_fast_path`) -- never from a call that merely
    /// started and finished with no card, which was also true of a call that failed the CLI's own
    /// validation before any gate (whole-branch review finding 2). A card naming this id later kept
    /// for the same call drops it (the CLI's own prompt after the gate); so does a later card naming
    /// NO id at all whose own `(tool_name, input)` matches (review item 2, the same fix as
    /// `rule_candidates`'s own). A `Resync` turns one whose call finished among the dropped events
    /// into its note. Cleared with `prompt_note_candidates`.
    auto_edit_candidates: std::collections::BTreeMap<String, CandidateCall>,
    /// Tool-use ids already in `notes.auto_creates_file` that no events payload has carried yet
    /// (whole-branch review finding 6). The note is made when the gate is answered, and a gate
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
    /// Permission ids this tab answered `allow` on the user's behalf -- in bypass
    /// (`approve_pending`), or under the classifier (`answer_what_needs_no_human`) -- and is still
    /// waiting on the provider's `PermissionResolved` for (R07/S2, D9). Hidden from the snapshot
    /// (`SnapshotView::of`) and from the tray (`pump`'s `resync`/attention feed), so a request
    /// nobody needs to see never draws a card and never counts. Private: only `tab_set` may insert,
    /// so the hiding rule cannot be bypassed from outside this module. Read directly by this
    /// module's own tests.
    host_answered: BTreeSet<String>,
    /// The calls the user approved on a card in this tab, this turn, each with the tool and input the
    /// card showed (O3 ruling 5): the CLI's own prompt for exactly such a call, which follows the
    /// gate's request for it, is answered `allow` without a second card, once -- as the real CLI asks
    /// once. Written only by `TabSet::answer_card` on an Approve that reached the provider; emptied
    /// when the turn ends (the CLI asks within the turn that made the call), on EVERY `Resync` (one
    /// can swallow a turn boundary -- Codex's finding), on `r` and with the backend. Private for the
    /// reason `host_answered` is.
    human_allowed: crate::agent_backend::HumanApprovals,
    /// Tool-use id -> the note for a call whose CLI prompt was answered without a card (review item
    /// 7), until the call completes; it then becomes `notes.prompt_notes`, as a rule candidate does.
    prompt_note_candidates: std::collections::BTreeMap<String, String>,
    /// Permission ids the user answered on a card here (`TabSet::answer_card`, any decision) that
    /// the projection still lists as pending -- on the sidecar path until the provider's own
    /// `PermissionResolved` arrives (`AgentConversation::respond_permission`'s doc). `refresh_offers`
    /// never offers a rule for one again, and [`Tab::card_is_waiting`] says no, so a second response
    /// for the same id (a replay after a tab switch or a page reload) can never save a rule: the
    /// v1-hardening whole-branch review found a one-time `rule_offers.remove` undone by the next
    /// pump that saw another id arrive. Pruned with `host_answered` once the id leaves the
    /// projection. Private for the reason `host_answered` is.
    user_answered: BTreeSet<String>,
}

impl Tab {
    fn new(id: TabId, number: u16, mode: SessionModeChoice) -> Self {
        Tab {
            id,
            number,
            name: None,
            title: None,
            mode,
            backend: TabBackend::NotStarted,
            attention: AttentionTracker::default(),
            stale: false,
            turn_trace: None,
            reported_start_failure: false,
            pending_handoff: None,
            last_handoff: None,
            queue: Vec::new(),
            queue_error: None,
            draft: String::new(),
            rule_offers: std::collections::BTreeMap::new(),
            editing_draft: None,
            was_running: false,
            rule_offers_seen: Vec::new(),
            notes: CallNotes::default(),
            rule_candidates: std::collections::BTreeMap::new(),
            auto_edit_candidates: std::collections::BTreeMap::new(),
            creates_file_unsent: BTreeSet::new(),
            turn_clock: None,
            host_answered: BTreeSet::new(),
            human_allowed: crate::agent_backend::HumanApprovals::default(),
            prompt_note_candidates: std::collections::BTreeMap::new(),
            user_answered: BTreeSet::new(),
        }
    }

    /// Whether `permission_id` is a card still waiting for the user here: its live backend's
    /// projection lists it as pending and the user has not already answered it (`user_answered`).
    /// What the panel's "Always allow" checks before saving a rule (panel-content review finding 4).
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
            _ => None,
        }
    }

    pub fn turn_running(&self) -> bool {
        self.live()
            .is_some_and(|backend| backend.projection().active_turn_id.is_some())
    }

    pub fn wire_state(&self) -> TabStateWire {
        match &self.backend {
            TabBackend::NotStarted => TabStateWire::NotStarted,
            TabBackend::Starting(_) => TabStateWire::Starting,
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
            title: self.title.clone(),
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
    /// queues (ruling 3a).
    pub turn_ended: Vec<TabId>,
    /// Tabs whose `rule_offers` changed in this tick: the caller sends them to the panel.
    pub offers_changed: Vec<TabId>,
    /// Tabs this tick failed because their CLI reported an ungated permission mode (spec §2.3,
    /// D12), each with the backend taken out of it. The caller shuts each one down off the GTK
    /// thread exactly as it does a closed tab's (`shell::agent_panel`'s `Retiring::backend`).
    pub tripped: Vec<Tripped>,
}

/// One tab the CLI-mode tripwire failed (spec §2.3, D12). See `PumpOutput::tripped`.
pub struct Tripped {
    pub tab: TabId,
    /// The same text the tab's `Failed` reason carries.
    pub reason: String,
    /// The session, still running until the caller shuts it down: its shutdown is what denies the
    /// requests left pending in it.
    pub backend: AgentBackend,
}

/// Why a tab whose CLI reported `reported` was closed (spec §2.3's text). Names the setting most
/// likely responsible, because that is the one thing the user can go and change.
pub fn ungated_cli_mode_reason(reported: &str, detail: &str) -> String {
    format!(
        "the CLI reports permission mode '{reported}' ({detail}) — a project's permissions.defaultMode? \
         Eitri runs only sessions it gates itself; the session was closed"
    )
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
    /// Already open in this window: only ever switched to (spec §3.6).
    SwitchTo(TabId),
    /// Held by another window's lease: nothing is started (spec §3.9).
    Refuse(String),
    /// Start the resume in this tab (an empty current tab, or a new one).
    StartIn(TabId),
}

/// Which mode a bypass entry moves: one tab's own mode, or the window's remembered `default_mode`
/// for tabs not yet started (R06/S2, D2/D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassScope {
    Tab(TabId),
    Default,
}

/// A pending "enter bypass" prompt, stored on `TabSet::pending_bypass` until `y`/`n` answers it (D1,
/// D2, D11). `approve` never reaches the panel (`serialize_confirm_bypass_for_js` sends only
/// `lines`) -- Rust keeps the list so the panel never counts cards or echoes an id back (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BypassPlan {
    pub scope: BypassScope,
    /// Answers a stale confirm (D7): only the exact prompt shown, by this nonce, may be accepted.
    pub nonce: u64,
    /// The delivered cards this entry would approve if confirmed now, `seq`-ordered -- the same set
    /// `confirm_bypass` re-derives fresh (`waiting_cards`) and compares against before acting.
    pub approve: Vec<String>,
    /// What the panel shows (`tabs::bypass_prompt`), one or more lines (D11's y/n plus any warning).
    pub lines: Vec<String>,
    /// Which of `tabs::bypass_prompt`'s texts `lines` is -- what the user is agreeing to. A live
    /// tab's prompt says nothing about new sessions; an empty tab's says they will use bypass too.
    /// `confirm_bypass` re-derives it and reprompts when it no longer matches (the whole-branch
    /// review: a live tab handed off to a terminal became `NotStarted` under an open live-tab prompt,
    /// and its `y` then moved the window default into bypass, spec §7.2).
    pub prompt: tabs::PromptScope,
}

/// What `cycle_mode`/`cycle_default_mode` return: bypass -> auto moves at once (D6); auto -> bypass
/// always needs a confirm first (D2), never applied here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeCycle {
    Changed(SessionModeChoice),
    Confirm(BypassPlan),
}

/// What `confirm_bypass` did with a stored `BypassPlan`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmOutcome {
    /// Entered bypass, having approved `approved` of the cards the plan listed. `resolved` is what
    /// answering them produced on this side (`AgentBackend::approve_pending`'s `Approved::events`:
    /// legacy's `PermissionResolved`s, which no pump will ever carry) -- the caller owes the panel
    /// these, or the approved cards stay drawn (Codex v1-mode finding 1). Empty on the sidecar.
    Entered {
        approved: usize,
        resolved: Vec<AgentDomainEvent>,
    },
    /// The tab was already in bypass by the time this confirm arrived (a race, not an error):
    /// nothing to do.
    AlreadyBypass,
    /// A delivered card arrived after the plan was built and is not in `approve` (D7): nothing
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
}

impl RestoreGo {
    /// The sessions to start, in order: what a caller that takes its leases ahead of time needs.
    pub fn session_ids(&self) -> Vec<String> {
        self.tabs
            .iter()
            .map(|t| t.planned.provider_session_id.clone())
            .collect()
    }
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
    /// An "enter bypass" prompt on screen, if any (D2): at most one at a time, a fresh press on the
    /// mode key (or any tab-switching call) superseding or dropping whatever was open.
    pending_bypass: Option<BypassPlan>,
    /// The next `BypassPlan::nonce`, starting at 1 so `0` is never a valid nonce to compare against.
    next_nonce: u64,
    /// Every card the tabs removed from this set had been handed, so the window's `arrived`
    /// (`attention`) never goes down when a tab closes: [`crate::attention::react`] reads only its
    /// growth.
    removed_arrived: u64,
    /// The project's D7 prefix rules, window-level: every tab's pump applies them (ruling 17).
    rules: agent::PrefixRules,
    /// The tabs that were active, least recent first and each once: where the saved active tab falls
    /// back to when the active one has no session to save.
    recency: Vec<TabId>,
    /// A restore's bypass question on screen, if any: at most one prompt at a time, as `pending_bypass`.
    pending_restore: Option<PendingRestore>,
    /// Set once the window has taken the tabs away to close them (`take_all`). A set that has none
    /// left must not be mistaken for a user who closed every tab.
    closing: bool,
    /// Test seam (P1-A2 round 2): run once, right after `pump`'s next drain of a live tab's queue,
    /// so a test folds an event exactly where the race is -- after a drain and before the next
    /// snapshot read, `active_state_payloads`' or the `Resync` arm's own a few lock cycles later --
    /// rather than hoping a timing probe lands there.
    #[cfg(test)]
    after_drain: Option<BackendSeam>,
    /// Test-only: deliver what a snapshot already carried as well, the semantics before P1-A2 round
    /// 2, so a test can show what leaving it out prevents (and record both for the reducer test).
    #[cfg(test)]
    redeliver_covered: bool,
    /// Test seam (P1-A2 round 4): run once, right before the `Resync` arm's own snapshot read --
    /// after its early turn-clock read and its own further lock cycles (the three `projection()` calls
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
    /// **Never in bypass** (D3, R07/S2; the whole-branch review): a `Bypass` here is taken as `Auto`,
    /// with a log line, so "a launch never starts in bypass" holds by construction rather than
    /// because the one caller (`agent_panel`, passing `agent_prefs::startup_mode`) happens never to
    /// pass it. Without this, every tab of the window -- tab 1 and every later `open()` -- would sit
    /// in bypass with no `y` ever pressed, the one thing `Tab::mode` being private exists to stop.
    /// The one way a window starts in bypass is the user's own `init.lua`
    /// ([`TabSet::apply_configured_default`]), a separate call that says so.
    pub fn new(kind: BackendKind, default_mode: SessionModeChoice) -> Self {
        let default_mode = match default_mode {
            SessionModeChoice::Bypass => {
                eprintln!("[permission] a window never starts in bypass (D3): new tabs start in auto");
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
            removed_arrived: 0,
            rules: agent::PrefixRules::default(),
            recency: Vec::new(),
            pending_restore: None,
            closing: false,
            #[cfg(test)]
            after_drain: None,
            #[cfg(test)]
            redeliver_covered: false,
            #[cfg(test)]
            before_resync_snapshot: None,
        };
        set.open();
        set.last_active = None;
        set
    }

    pub fn kind(&self) -> BackendKind {
        self.kind
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

    /// Ruling 17: the rules apply to every tab from the next pump; a card already on screen is not
    /// answered by a rule added after it arrived.
    pub fn set_rules(&mut self, rules: agent::PrefixRules) {
        self.rules = rules;
    }

    pub fn rules(&self) -> &agent::PrefixRules {
        &self.rules
    }

    /// `prefix c`: a new empty tab at the lowest free number, selected.
    pub fn open(&mut self) -> TabId {
        let used: Vec<u16> = self.tabs.iter().map(|t| t.number).collect();
        let id = TabId(self.next_id);
        self.next_id += 1;
        self.tabs
            .push(Tab::new(id, tabs::lowest_free_number(&used), self.default_mode));
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
            // Spec §3.3: every method that MOVES the active tab drops a pending bypass prompt --
            // it was about the tab the user is leaving, and a `y` typed after switching away must
            // never land on it.
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

    /// `n` (+1) / `p` (-1), in number order, wrapping (ruling 10).
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
    /// shut down or watch. The last tab's removal opens a fresh tab 1 (spec §3.5).
    pub fn remove(&mut self, id: TabId) -> Option<Tab> {
        let at = self.tabs.iter().position(|t| t.id == id)?;
        let before = self.active;
        let tab = self.tabs.remove(at);
        self.removed_arrived += tab.attention.attention().arrived;
        self.recency.retain(|t| *t != id);
        if self.last_active == Some(id) {
            self.last_active = None;
        }
        if self.tabs.is_empty() {
            self.open();
            self.last_active = None;
        } else if self.active == id {
            let fallback = self.tabs[at.min(self.tabs.len() - 1)].id;
            self.active = self.last_active.take().unwrap_or(fallback);
            let now_active = self.active;
            self.note_recent(now_active);
        }
        // `open()` above already dropped it through `select`; this covers the fallback path, which
        // assigns `self.active` directly (spec §3.3).
        if self.active != before {
            self.drop_bypass_prompt();
        }
        Some(tab)
    }

    /// `prefix ,`: normalized (ruling 11) and handed to a live sidecar's record (Task 2). A tab that
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

    /// `Shift+Tab` / `<leader>m`: the auto/bypass TOGGLE, frozen as such (O2 a) -- an explicit
    /// `match` on the mode, never `SessionModeChoice::cycled` (that helper is Claude Code's N-way
    /// cycle order and is not what this key does any more). Leaving bypass moves at once, in every
    /// tab state including `ended`/`failed` (D6). Entering it never applies here: it always needs a
    /// `confirm_bypass` first (D2), so this returns a plan to show rather than a changed mode.
    pub fn cycle_mode(&mut self, id: TabId) -> Result<ModeCycle, String> {
        // A fresh press supersedes whatever prompt was already open (D2); `Confirm` below stores a
        // new one if it builds one.
        self.pending_bypass = None;
        self.pending_restore = None;
        let at = self
            .tabs
            .iter()
            .position(|t| t.id == id)
            .ok_or_else(|| format!("no tab {}", id.0))?;
        match self.tabs[at].mode {
            SessionModeChoice::Bypass => {
                let tab = &mut self.tabs[at];
                tab.mode = SessionModeChoice::Auto;
                let number = tab.number;
                let not_started = matches!(tab.backend, TabBackend::NotStarted);
                // Ruling 5 / D13: an empty tab's own move also carries the window default with it
                // (there is no session yet for the mode to be "just this tab's own"), and leaving
                // bypass on ANY tab returns an in-window default to auto, wherever that default came
                // from -- a live tab someone confirmed into bypass while the default was also
                // bypass must not leave the default stranded there once it itself leaves.
                let default_moved = not_started || self.default_mode == SessionModeChoice::Bypass;
                if default_moved {
                    self.default_mode = SessionModeChoice::Auto;
                }
                eprintln!(
                    "[permission] mode is now auto (tab {number}{})",
                    if default_moved { "; new sessions too" } else { "" }
                );
                Ok(ModeCycle::Changed(SessionModeChoice::Auto))
            }
            SessionModeChoice::Auto => {
                let tab = &self.tabs[at];
                match tab.wire_state() {
                    TabStateWire::Ended | TabStateWire::Failed => Err("the session has ended".to_string()),
                    _ if id != self.active => Err(format!("tab {} is not the one on screen", tab.number)),
                    _ => {
                        let scope_kind = if matches!(tab.backend, TabBackend::NotStarted) {
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
                            lines: bypass_lines(scope_kind, tab, approve.len()),
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
    /// `NotStarted` (spec §6.3, panel round 2 plan Task 5): the same auto/bypass toggle as
    /// `cycle_mode`, applied to the window's remembered `default_mode` rather than any one tab's own
    /// mode. Every open tab keeps whatever mode it already has.
    pub fn cycle_default_mode(&mut self) -> ModeCycle {
        self.pending_bypass = None;
        self.pending_restore = None;
        match self.default_mode {
            SessionModeChoice::Bypass => {
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

    /// `y`/`Y` to a `confirm_bypass` prompt (D1, D2, D7, D11). Compares against the STORED plan
    /// before taking it: a mismatched scope or a stale nonce leaves the CURRENT prompt exactly as it
    /// was, so an answer to an old prompt can never cancel a newer, still-open one.
    pub fn confirm_bypass(&mut self, scope: BypassScope, nonce: u64) -> Result<ConfirmOutcome, String> {
        match &self.pending_bypass {
            Some(plan) if plan.scope == scope && plan.nonce == nonce => {}
            _ => return Err("that prompt is no longer current".to_string()),
        }
        let plan = self.pending_bypass.take().expect("checked above");
        match scope {
            BypassScope::Tab(id) => {
                let at = match self.tabs.iter().position(|t| t.id == id) {
                    Some(at) => at,
                    None => return Err(format!("no tab {}", id.0)),
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
                // D7: re-derive what `y` would approve NOW, not what the plan said when it was
                // built -- a card delivered while the prompt was up must never be silently approved.
                let now = waiting_cards(&self.tabs[at]);
                // And re-derive WHICH question it is: the tab can change kind under an open prompt
                // (a terminal handoff turns a live tab into `NotStarted`), and an empty tab's `y`
                // also moves the window default -- which a live tab's prompt never said (spec §7.2:
                // the only way into bypass is a confirmed prompt or a confirmed window default).
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
                        lines: bypass_lines(scope_kind, &self.tabs[at], now.len()),
                        approve: now,
                        prompt: scope_kind,
                    };
                    self.pending_bypass = Some(fresh.clone());
                    return Ok(ConfirmOutcome::Reprompt(fresh));
                }
                // `now` is already a subset of `plan.approve` (checked above), so it IS the
                // intersection D7 asks for -- nothing besides a still-pending, still-delivered card
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
                tab.host_answered.extend(answered.ids.iter().cloned());
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
    /// human decision takes, so the tab can remember which calls the user approved (O3 ruling 5).
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
        // Panel-content review finding 4: the card is answered, whatever the decision, so its rule
        // offer goes now and is never made again (`user_answered`'s doc). Only on success: a refused
        // answer leaves the card open and its offer with it (ruling 16).
        tab.rule_offers.remove(permission_id);
        tab.user_answered.insert(permission_id.to_string());
        Ok(events)
    }

    /// Clears whatever "enter bypass" prompt is on screen (D2, spec §3.3). Every method that moves
    /// the active tab calls this once the active id has actually changed, so a stale `y` typed after
    /// switching tabs, opening a new one, or removing one can never land on a prompt about a tab the
    /// user is no longer looking at. Task 3 additionally calls it when the panel loses focus.
    pub fn drop_bypass_prompt(&mut self) {
        self.pending_bypass = None;
        self.pending_restore = None;
    }

    /// `r` (ruling 12): an ended or failed tab back to empty, keeping its number, name and mode.
    pub fn reset(&mut self, id: TabId) -> Result<Option<AgentBackend>, String> {
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        match tab.wire_state() {
            TabStateWire::Ended | TabStateWire::Failed => {}
            _ => return Err("only an ended or failed tab starts over".to_string()),
        }
        // Ruling 9: the queue and the unsent draft come back as the draft, oldest first.
        let mut parts: Vec<String> = std::mem::take(&mut tab.queue).into_iter().map(|q| q.text).collect();
        if !tab.draft.trim().is_empty() {
            parts.push(std::mem::take(&mut tab.draft));
        }
        tab.draft = parts.join("\n\n");
        tab.queue_error = None;
        tab.rule_offers.clear();
        tab.rule_offers_seen.clear();
        tab.user_answered.clear();
        tab.was_running = false;
        let old = std::mem::replace(&mut tab.backend, TabBackend::NotStarted);
        tab.title = None;
        tab.attention.restart();
        tab.stale = false;
        tab.turn_trace = None;
        tab.reported_start_failure = false;
        // The backend that answered these is gone with `old`; nothing left for the set to hide.
        // `tab.mode` itself is untouched -- a bypass tab resets back to an empty bypass tab (spec
        // §3.1's `r` row, D6).
        tab.host_answered.clear();
        // The tab changes kind (ended/failed -> `NotStarted`) under whatever prompt was open; drop it
        // rather than let a `y` answer a question about the tab as it was (the whole-branch review,
        // symmetric with the terminal handoff's own drop in `agent_panel`). `confirm_bypass`'s kind
        // re-check would reprompt anyway; this is the belt to that brace.
        tab.human_allowed.clear();
        tab.prompt_note_candidates.clear();
        tab.auto_edit_candidates.clear();
        tab.creates_file_unsent.clear();
        self.pending_bypass = None;
        Ok(match old {
            TabBackend::Live(backend) => Some(backend),
            _ => None,
        })
    }

    /// Ruling 2.
    pub fn resolve(&self, tab_ref: TabRef) -> Result<Option<TabId>, String> {
        match tab_ref {
            TabRef::WindowLevel => Ok(None),
            TabRef::Missing => Err("protocol: this command names no tab".to_string()),
            TabRef::Named(id) if self.get(id).is_some() => Ok(Some(id)),
            TabRef::Named(id) => Err(format!("protocol: no tab {}", id.0)),
        }
    }

    pub fn tab_with_session(&self, provider_session_id: &str) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|t| t.provider_session_id().as_deref() == Some(provider_session_id))
            .map(|t| t.id)
    }

    /// Every Claude session this window has open or just handed off: filtered out of `hello`
    /// (ruling 17) and of the chooser's records.
    pub fn open_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.tabs.iter().filter_map(|t| t.provider_session_id()).collect();
        ids.extend(
            self.tabs
                .iter()
                .filter_map(|t| t.last_handoff.as_ref().map(|c| c.provider_session_id().to_string())),
        );
        ids
    }

    pub fn route_resume(&mut self, from: TabId, provider_session_id: &str, held_elsewhere: bool) -> ResumeRoute {
        if let Some(open) = self.tab_with_session(provider_session_id) {
            self.select(open);
            return ResumeRoute::SwitchTo(open);
        }
        if held_elsewhere {
            return ResumeRoute::Refuse("open in another window".to_string());
        }
        let empty = self
            .get(from)
            .is_some_and(|t| matches!(t.backend, TabBackend::NotStarted));
        let target = if empty { from } else { self.open() };
        ResumeRoute::StartIn(target)
    }

    /// One 33 ms tick over EVERY tab (spec §3.1). The active tab's delivery is serialized for the
    /// panel; a background tab's feeds its attention and marks it stale, and nothing of it is sent.
    pub fn pump(&mut self, project_root: &Path, panel_mapped: bool) -> PumpOutput {
        let active = self.active;
        let mut out = PumpOutput {
            active_payload: None,
            active_class: crate::panel_cadence::EnvelopeClass::Immediate,
            first_text: false,
            turn_ended: Vec::new(),
            offers_changed: Vec::new(),
            tripped: Vec::new(),
        };
        let rules = &self.rules;
        for tab in &mut self.tabs {
            let is_active = tab.id == active;
            let on_screen = panel_mapped && is_active;
            let TabBackend::Live(backend) = &mut tab.backend else {
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
            let mut turn_ended = false;
            // Whether this tab's payload goes to the panel this tick (then `shell` prints a complete
            // trace after dispatching it; otherwise this function does, below).
            let mut dispatched = false;
            // `tab.mode` (R07/S2, spec §2.2, D9): the classifier's `Auto` path, or bypass answering
            // everything, and `tab.host_answered` collects every id either path answers so the
            // Resync arm and `active_state_payloads` can hide them.
            // `tab.human_allowed` (O3 ruling 5): in `Auto`, the CLI's own prompt for exactly a call
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
            // What the last snapshot of this backend the panel was sent already carried (P1-A2 round
            // 2): a snapshot and the drain before it are separate lock acquisitions, and the sidecar's
            // ingestion thread folds and queues in between, so such an event is in that snapshot AND
            // in this drain. Every queued event carries the revision its own fold produced, and the
            // fold, the queueing and the snapshot's read of `last_revision` each happen under the one
            // ingestion mutex: an event tagged at or below the snapshot's revision is exactly one it
            // carried, and all of them were queued before it was read -- so this drain holds every
            // one. Taken here, so nothing later compares against it; kept by the backend
            // (`AgentBackend::note_ui_snapshot`), so it never outlives the session it was read from.
            let covered = backend.take_ui_snapshot_revision();
            #[cfg(test)]
            let covered = covered.filter(|_| !self.redeliver_covered);
            tab.prompt_note_candidates.extend(
                answered_for_you
                    .prompts
                    .into_iter()
                    .map(|answered| (answered.tool_use_id, answered.note)),
            );
            // Whole-branch review findings 2 and 6, and the same fix applied to the pre-existing F18
            // rule note (fix round 3): what the fast path answered, what a saved rule answered, and
            // each `Write` answered without a card over no file, all as recorded at the answer
            // itself. The first two wait for their call to complete (`note_auto_edit_answers`,
            // `note_rule_answers`); the third is true already, and waits only for a payload to carry
            // it (`creates_file_unsent`).
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
            // The CLI-mode tripwire (spec §2.3, D12), before anything of this delivery reaches the
            // panel or the attention count: from the batch itself, or -- for a report a `Resync`
            // dropped, or one folded ahead of its delivery -- from the projection's record. The
            // whole batch is withheld, so no card in it is ever drawn to be approved; the session
            // is handed out to be shut down, which denies what is still pending in it.
            let reported = match &delivery {
                RevisedDelivery::Events(events) => events.iter().find_map(|(_, event)| match event {
                    AgentDomainEvent::UngatedCliMode { reported, detail } => Some((reported.clone(), detail.clone())),
                    _ => None,
                }),
                _ => None,
            };
            let reported = reported.or_else(|| {
                let record = backend.projection().ungated_cli_mode.clone();
                record.map(|r| (r.reported, r.detail))
            });
            if let Some((reported, detail)) = reported {
                let reason = ungated_cli_mode_reason(&reported, &detail);
                eprintln!("[permission] tab {}: {reason}", tab.id.0);
                tab.attention.session_ended(on_screen);
                tab.was_running = false;
                tab.turn_clock = None;
                let dead = std::mem::replace(&mut tab.backend, TabBackend::Failed { reason: reason.clone() });
                if let TabBackend::Live(backend) = dead {
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
            observe_turn_clock(
                &mut tab.turn_clock,
                backend.projection().active_turn_id.as_deref(),
                wall_clock_ms(),
            );
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
                            // The session ended: its queue waits for `r` (ruling 9), not a flush.
                            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. } => {
                                tab.was_running = false
                            }
                            // `Tab::mode` is the only authority (spec §2.2): nothing a provider
                            // reports moves it. A `PermissionModeChanged` that got this far named
                            // `default` (anything else became `UngatedCliMode`, handled above), so
                            // there is nothing to do with it.
                            //
                            // A resolution for an id this pump (or an earlier one) already answered
                            // on the user's behalf: forgotten now, since there is nothing left to
                            // hide it from (D9).
                            AgentDomainEvent::PermissionResolved { permission_id, .. } => {
                                tab.host_answered.remove(permission_id);
                            }
                            _ => {}
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
                    // The events are gone; the snapshot is what the panel is shown now.
                    //
                    // This arm drains (`take_revised_ui_delivery`, above) and then reads a fresh
                    // `SnapshotView::of` (below, for `is_active`) as two separate lock acquisitions
                    // on `Mutex<IngestState>`, with `approve_pending`'s own lock cycle for a bypass
                    // tab in between (answering a permission must happen outside the lock). A batch
                    // folded in that gap is queued for the next tick AND already in the snapshot
                    // sent here: the snapshot's revision, read under the guard it is serialized from,
                    // is noted on the backend, and the next drain leaves that batch out of the
                    // payload (P1-A2 round 2), exactly as after `active_state_payloads`' snapshot.
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
                        // Whole-branch review finding 2: a fast-path answer is a fact, recorded when
                        // it was given, so a call that finished among the dropped events becomes its
                        // note here the same way -- it used to linger as a candidate for good.
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
                        // The same fix, applied to the pre-existing F18 rule note (this task, v1
                        // trial whole-branch review, fix round 3): a rule's own answer is a fact
                        // recorded when it was given, so a call that finished among the dropped
                        // events becomes its note here too. Before this there was no analogous sweep
                        // at all for `rule_candidates`, so such a candidate lingered here forever --
                        // the still-pending sweep below only catches a permission genuinely still
                        // pending, never a call that already finished.
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
                    // Fix round finding 2: a candidate whose removing `PermissionRequested` was
                    // itself among the dropped events is not swept above (`prompt_note_candidates`'
                    // sweep only catches a candidate whose call ALSO finished inside the same drop)
                    // -- so it can survive a resync as a live candidate while the projection already
                    // shows its permission still pending, about to be drawn as a real card by the
                    // snapshot below. `answer_what_needs_no_human` never runs on a dropped batch (see
                    // its own doc), so nothing here was silently fast-pathed: whatever answers this
                    // card from here is a human, in every mode. Removed rather than turned into a
                    // note, the same as the event this stands in for would have done -- left in
                    // place, the human's own later approval would complete the call through the
                    // ordinary path and wrongly read "allowed by auto" for a call they answered on
                    // screen.
                    //
                    // v1 trial item 7 review, follow-up: `rule_candidates` has the identical shape --
                    // its own `PermissionRequested` removal step (`note_rule_answers`) is exactly as
                    // droppable, and left alone it would wrongly read "allowed by rule" instead. Swept
                    // the same way, by id rather than by value.
                    //
                    // Whole-branch review finding 2: a request this tab answered for the user
                    // (`host_answered` -- the fast path's, a rule's, bypass's) can still be listed as
                    // pending until the provider's own `PermissionResolved` is folded, and is no card
                    // (the snapshot below hides it): it is left out, or a fast-path answer recorded
                    // at the answer itself would be dropped by a `Resync` that merely came early.
                    // Since then an auto candidate exists only once the fast path answered its gate,
                    // so the card this still catches for one is the CLI's own later prompt for the
                    // same call. **The same has been true of a rule candidate since this task's own
                    // fix** (it used to exist from a `ToolCallStarted` alone, before any gate was
                    // answered) -- so the card this still catches for one is exactly the same shape,
                    // the CLI's own later prompt for the same call. The `host_answered` filter itself
                    // is load-bearing for both -- without it, a candidate whose own gate this tab just
                    // answered, but whose resolution the provider has not folded yet, would wrongly
                    // count as still needing a human and be swept away before its call even finishes:
                    // `a_resync_before_the_call_finishes_keeps_its_fast_path_answer` and
                    // `a_resync_before_the_call_finishes_keeps_its_rule_answer` each fail with it
                    // removed (checked by mutation in this task).
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
                    // D9: a resync in bypass sweeps whatever the overflow dropped through the same
                    // `approve_pending` a confirm uses, so a request never drawn as a card is never
                    // rebuilt as one by the very mechanism meant to rebuild the panel honestly. This
                    // also hides what the classifier already answered in `Auto`, closing the gap
                    // `attention`'s module doc used to record for `resync` itself.
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
                            // Observed from this snapshot's own view, under the guard it is
                            // serialized from -- not the early read above `match delivery`, which
                            // this arm's own further lock cycles (the three `projection()` reads
                            // above and `approve_pending`'s bypass sweep) can leave stale: if a
                            // turn ends and the next starts in that gap, the early read still names
                            // the first turn while this snapshot already names the second, and the
                            // panel takes the stamped time as exact (P1-A2 round 4; mirrors the same
                            // fix in `active_state_payloads`, whose own snapshot has the identical
                            // shape).
                            observe_turn_clock(
                                &mut tab.turn_clock,
                                view.projection.active_turn_id.as_deref(),
                                wall_clock_ms(),
                            );
                            let payload = serialize_snapshot_with_notes_for_js(
                                tab.id,
                                &view,
                                tab.turn_clock.as_ref().map(|(_, at)| *at),
                                &tab.notes,
                            );
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
            // A trace with no dispatch to wait for is printed here once it finishes: a background
            // tab's, and the active tab's when nothing of it reached the panel this tick -- its end
            // may have arrived only inside a snapshot (P1-A2 round 2). A dispatched one is printed
            // by `shell`, after it stamps the dispatch.
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
            // `turn_running()` guards a turn that has already started again (a send that raced the
            // completion): the queue waits for that one's end instead.
            if turn_ended && !tab.turn_running() {
                out.turn_ended.push(tab.id);
            }
        }
        out
    }

    /// Collects every connect that finished. An installed backend clears the tab's previous
    /// handoff and attention, takes the resumed title, and is told the tab's rename.
    pub fn collect_starts(&mut self) -> Vec<StartCollected> {
        let mut collected = Vec::new();
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
                    tab.backend = TabBackend::Live(backend);
                    tab.was_running = false;
                    tab.attention.restart();
                    tab.notes = CallNotes::default();
                    tab.rule_candidates.clear();
                    tab.auto_edit_candidates.clear();
                    tab.creates_file_unsent.clear();
                    tab.prompt_note_candidates.clear();
                    tab.last_handoff = None;
                    tab.reported_start_failure = false;
                    tab.title = pending.resumed_title;
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
        collected
    }

    /// After a switch or on `ready`: the active tab's own state. Its snapshot if it has a backend, or
    /// its handoff card if it has one (ruling 13). Always sent, not only when stale (ruling 18).
    ///
    /// **Reads; never drains** (Codex audit P1-A2 and its round-2 review). The sidecar's ingestion
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
                        view.projection.active_turn_id.as_deref(),
                        wall_clock_ms(),
                    );
                    let payload = serialize_snapshot_with_notes_for_js(
                        tab.id,
                        &view,
                        tab.turn_clock.as_ref().map(|(_, at)| *at),
                        &tab.notes,
                    );
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
        let resumable = self.kind == BackendKind::Sidecar;
        let views: Vec<TabView> = self.tabs.iter().map(|t| t.view(resumable)).collect();
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

    /// Ruling 15: a turn in progress, or a connect.
    pub fn running_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t.backend, TabBackend::Starting(_)) || t.turn_running())
            .count()
    }

    /// `<leader>bd` (wave 4, R1): vim's `:bd` closes a buffer at once unless it has changes to lose (E89); here the
    /// things to lose are a running turn, a connect in flight, and queued messages -- the facts `close_prompt`
    /// names. `prefix &` does not ask this: it is tmux's `confirm-before` and always asks.
    pub fn close_needs_confirm(&self, id: TabId) -> bool {
        self.get(id)
            .is_some_and(|t| matches!(t.backend, TabBackend::Starting(_)) || t.turn_running() || !t.queue.is_empty())
    }

    pub fn close_facts(&self, id: TabId) -> Option<tabs::CloseFacts> {
        let tab = self.get(id)?;
        Some(tabs::CloseFacts {
            number: tab.number,
            label_name: tab.label_name(),
            turn_running: tab.turn_running(),
            queued: tab.queue.len(),
            legacy: self.kind == BackendKind::Legacy,
            has_backend: tab.live().is_some(),
        })
    }

    /// `<leader>bo` (Owner answers Q2): the ids `tabs::close_others` would close, and the y/n
    /// naming them, over every tab but the active one. Same "running" test as [`Self::running_count`]
    /// (a turn in progress, or still connecting). `None` when the active tab is the only one open.
    pub fn close_others_plan(&self) -> Option<(Vec<TabId>, String)> {
        let entries: Vec<(TabId, bool)> = self
            .tabs
            .iter()
            .map(|t| (t.id, matches!(t.backend, TabBackend::Starting(_)) || t.turn_running()))
            .collect();
        tabs::close_others(&entries, self.active())
    }

    /// Ruling 5. `wire` is already composed with the editor context of this moment (ruling 1).
    pub fn queue_message(&mut self, id: TabId, text: &str, wire: String, now_ms: u64) -> Result<usize, String> {
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        if tab.pending_handoff.is_some() {
            return Err("this tab is being handed off to a terminal".to_string());
        }
        match tab.wire_state() {
            TabStateWire::Starting | TabStateWire::Live => {}
            TabStateWire::NotStarted => return Err("nothing is running to queue behind; Enter sends".to_string()),
            TabStateWire::Ended | TabStateWire::Failed => {
                return Err("this tab's session has ended; press r to start over".to_string())
            }
        }
        tab.queue.push(Queued {
            text: text.to_string(),
            wire,
            queued_at_ms: now_ms,
        });
        Ok(tab.queue.len())
    }

    /// Ruling 3 and 4: one turn from the whole queue, only when the tab can take one.
    pub fn flush_queue(&mut self, id: TabId) -> Option<Flush> {
        let tab = self.get_mut(id)?;
        if tab.queue.is_empty() || tab.pending_handoff.is_some() || tab.turn_running() {
            return None;
        }
        let backend = tab.live_mut()?;
        if !backend.projection().pending_permissions.is_empty() {
            return None;
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
        let outcome = tab.live_mut().expect("checked above").send_turn(&wire, &typed);
        match &outcome {
            Ok(_) => {
                tab.queue.clear();
                tab.queue_error = None;
                tab.turn_trace = trace;
            }
            Err(error) => tab.queue_error = Some(error.message.clone()),
        }
        tab.was_running = tab.turn_running();
        Some(Flush { typed, outcome })
    }

    /// Ruling 8.
    pub fn send_now(&mut self, id: TabId, text: &str, wire: String, now_ms: u64) -> Result<SendNow, String> {
        if !text.trim().is_empty() {
            self.queue_message(id, text, wire, now_ms)?;
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

    /// Ruling 7: the whole queue, oldest first, for the panel to merge into the box.
    pub fn take_back_queue(&mut self, id: TabId) -> Vec<String> {
        let Some(tab) = self.get_mut(id) else { return Vec::new() };
        tab.queue_error = None;
        std::mem::take(&mut tab.queue).into_iter().map(|q| q.text).collect()
    }

    /// Ruling 6: the panel's mirror. Never echoed.
    pub fn set_draft(&mut self, id: TabId, text: &str) -> bool {
        let Some(tab) = self.get_mut(id) else { return false };
        tab.draft = text.to_string();
        true
    }

    /// A prompt was sent from `id`'s composer: whatever of it the panel had mirrored here is no
    /// longer a draft. The composer clears its own box on Enter and mirrors `""` 300 ms later, but a
    /// tab's state payloads (`active_state_payloads`, sent again when a first turn's connect
    /// finishes) carry this copy, and the panel adopts it over its own pending `""` -- so the text
    /// just sent came back into the box of the conversation it had started (wave-4 GUI pass,
    /// 2026-09-26). Cleared here, at the send, rather than waiting for the mirror.
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

    /// Every tab's queue, summed (the window-close prompt, ruling 10).
    pub fn queued_count(&self) -> usize {
        self.tabs.iter().map(|t| t.queue.len()).sum()
    }

    /// Ruling 10: the window is closing; every queued text, tab by tab, for the history.
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
    /// (`shell::agent_panel` checks `shutting_down` first on every path that reaches the set).
    pub fn take_all(&mut self) -> Vec<Tab> {
        self.closing = true;
        std::mem::take(&mut self.tabs)
    }

    /// Whether no tab of this window has started a session (or is starting one): the state a launch
    /// is in until something is typed or resumed.
    pub fn is_pristine(&self) -> bool {
        self.tabs.iter().all(|t| matches!(t.backend, TabBackend::NotStarted))
    }

    /// The tabs worth bringing back next launch (`saved_tabs`), or `None` while the window is closing
    /// and has handed its tabs over to be torn down -- closing every tab is not the user closing
    /// them. A tab is listed when it has a Claude session, which includes one still being resumed.
    /// Only the sidecar backend resumes, so a window on the legacy backend lists nothing.
    ///
    /// `conversation_id` is the project's (`agent::conversation_id_for_cwd`): every session of one
    /// project shares it, and a tab whose backend has not been built yet has none of its own.
    pub fn saved_snapshot(&self, conversation_id: &str) -> Option<SavedTabs> {
        if self.closing {
            return None;
        }
        if self.kind != BackendKind::Sidecar {
            return Some(SavedTabs::default());
        }
        // `provider_session_id` is read in its own statement per tab (the lock-order rule above).
        let listed: Vec<(TabId, SavedTab)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let session = tab.provider_session_id().filter(|id| !id.is_empty())?;
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
        let active = listed
            .iter()
            .position(|(id, _)| *id == self.active)
            .or_else(|| {
                self.recency
                    .iter()
                    .rev()
                    .find_map(|recent| listed.iter().position(|(id, _)| id == recent))
            })
            .unwrap_or(0);
        Some(SavedTabs {
            tabs: listed.into_iter().map(|(_, saved)| saved).collect(),
            active,
        })
    }

    /// The user's own `init.lua` choice of the mode new tabs start in (`agent.default_mode`): the
    /// window's default, and every empty tab it has so far. This is the one route into bypass that
    /// needs no question, because the question has been answered in a file the user wrote.
    pub fn apply_configured_default(&mut self, mode: SessionModeChoice) {
        self.default_mode = mode;
        for tab in &mut self.tabs {
            if matches!(tab.backend, TabBackend::NotStarted) {
                tab.mode = mode;
            }
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
        F: FnMut(&str) -> Result<mpsc::Receiver<Result<AgentBackend, BackendError>>, String>,
    {
        let total = go.tabs.len() + go.skipped.len();
        let mut run = RestoreRun::new(
            from,
            self.get(from).map_or(self.default_mode, |t| t.mode),
            total,
            go.skipped,
        );
        let mut started: Vec<(String, TabId)> = Vec::new();
        for (n, granted) in go.tabs.into_iter().enumerate() {
            let GrantedTab { planned, mode } = granted;
            let id = planned.provider_session_id.clone();
            if let Some(open) = self.tab_with_session(&id) {
                run.restored += 1;
                started.push((id, open));
                continue;
            }
            let result_rx = match connect(&id) {
                Ok(rx) => rx,
                Err(reason) => {
                    run.failed.push(Skipped {
                        label: planned.label,
                        reason,
                    });
                    continue;
                }
            };
            let ResumeRoute::StartIn(target) = self.route_resume(from, &id, false) else {
                continue;
            };
            let tab = self.get_mut(target).expect("route_resume names a tab it has");
            tab.mode = mode;
            tab.backend = TabBackend::Starting(PendingStart {
                request_id: format!("restore-{}", n + 1),
                result_rx,
                first_turn: None,
                resume: Some(id.clone()),
                resumed_title: planned.title,
                resumed_name: planned.name,
            });
            run.pending.insert(target, planned.label);
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

    /// A restored tab whose resume failed. `back_to` is [`RestoreRun::back_to`]: an empty tab again, in
    /// the mode it had before the restore, if it is the one the restore began in; gone if the restore
    /// made it. What went wrong is reported once for the whole restore, not left as a dead tab each.
    pub fn discard_failed_restore(&mut self, tab: TabId, back_to: Option<SessionModeChoice>) {
        if !matches!(self.get(tab).map(|t| &t.backend), Some(TabBackend::Failed { .. })) {
            return;
        }
        match back_to {
            Some(mode) => {
                if let Some(t) = self.get_mut(tab) {
                    t.backend = TabBackend::NotStarted;
                    t.reported_start_failure = false;
                    t.mode = mode;
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
    }
}

/// The exact `(tool_name, input)` a rule's or the fast path's own answer saw, kept alongside a
/// candidate so a LATER card for the very same call -- but whose own `PermissionRequested` names
/// NO tool-use id -- can still be matched by content and drop the candidate (review item 2:
/// Verdandi's `permissionBroker.ts` sends `toolUseId: ''` when the CLI gave none, and `translate.rs`
/// maps that to `None`, so such a card has no id for the ordinary "same id" removal to find at
/// all). Equality is exact (`Value` equality, which ignores object key order), the same choice
/// `HumanApprovals` makes and for the same reason: small, short-lived sets, no canonical form to
/// get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidateCall {
    tool_name: String,
    input: serde_json::Value,
}

/// A saved prefix rule's own candidate (v1 polish F18): its display string, and the call its answer
/// saw, for the id-less-card match `CandidateCall`'s own doc describes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RuleCandidate {
    rule: String,
    call: CandidateCall,
}

/// A later `PermissionRequested` in this batch that drops a candidate: the ordinary case, naming
/// this exact tool-use id; or -- review item 2 -- one naming no id at all, whose own
/// `(tool_name, input)` matches a candidate's. The second case picks the first match in id order
/// (stable, since candidates are a `BTreeMap`); two identical concurrent calls answered the same way
/// could in principle collide here, matching the wrong one of the two, but each is `Bash`/`Write`/
/// `Edit`/`NotebookEdit` with identical arguments and an identical downstream note either way, so
/// nothing a row shows can actually differ.
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

/// v1 polish F18: which tool calls in this batch a saved prefix rule answered, as (tool-use id,
/// `Bash(git log *)`), recorded in the tab's `notes` for later snapshots too.
///
/// `candidates` holds only what `AgentBackend::answer_what_needs_no_human` reported it answered by
/// a saved rule, recorded at that answer (v1 trial whole-branch review, fix round 3 -- the same
/// fix as finding 2, applied to this pre-existing note). A `PermissionRequested` in this batch
/// naming one of them drops it: a card was delivered for the same call after all (the CLI's own
/// prompt after the gate), by id in the ordinary case and by content when the card names no id at
/// all (review item 2, `dropped_candidate_id`'s own doc). A `ToolCallCompleted` turns a surviving
/// candidate into a note. This used to make every `ToolCallStarted` a rule WOULD answer a
/// candidate, by re-asking `agent::rule_that_allows` on that event's own arguments -- before any
/// gate existed to answer -- and infer the answer from no card ever having arrived: also true of a
/// call the CLI's `validateInput` failed before any hook ran (no gate at all), and of a gate a
/// switch to bypass answered instead of the rule. Each read "allowed by rule <rule>" with nothing
/// having allowed it by that rule.
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

/// v1 trial item 7: which `Write`/`Edit`/`NotebookEdit` calls in this batch the acceptEdits fast
/// path (`agent::permission_policy`'s module doc, "The acceptEdits fast path") answered without a
/// card, as tool-use ids, recorded in the tab's `notes.allowed_by_auto` for later snapshots too.
///
/// `candidates` holds only what `AgentBackend::answer_what_needs_no_human` reported it answered by
/// the fast path, recorded at that answer (whole-branch review finding 2, 2026-09-28). A
/// `PermissionRequested` in this batch naming one of them drops it: a card was delivered for the
/// same call after all (the CLI's own prompt after the gate, `agent::permission_policy`'s module doc
/// on the fast path), so a human answered too -- by id in the ordinary case and by content when the
/// card names no id at all (review item 2, `dropped_candidate_id`'s own doc). A `ToolCallCompleted`
/// turns a surviving candidate into a note. This used to make every `ToolCallStarted` of those tools
/// in `Auto` a candidate and infer the answer from no card having been delivered, which was also
/// true of a call the CLI's `validateInput` failed before any hook ran, of a card whose request
/// named no tool-use id, and of a gate a switch to bypass answered: each read "allowed by auto" with
/// nothing having allowed it.
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

/// v1 polish F22: the `Write` cards in this batch whose file does not exist now, when the card is
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
/// The delivered cards this tab could approve right now (R06/S2, spec §3.3): its own attention
/// tray, intersected with the projection's still-pending ids and with anything it has not already
/// host-answered, less any CLI prompt the user's own ask rule forced (O3), `seq`-ordered (the order
/// the panel shows them in). Empty unless the tab is
/// `Live` -- a `NotStarted`/`Starting`/`Failed` tab has no session to hold a pending request at all.
///
/// This one function builds every `BypassPlan::approve` and every D7 re-check `confirm_bypass`
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
            .filter(|p| cards.contains(&p.permission_id) && !tab.host_answered.contains(&p.permission_id))
            // O3 ruling 4 / review #3: a CLI prompt only a human answers (an ask rule's, or one of
            // unknown kind) is a card in bypass too, so entering bypass neither counts nor approves
            // it (`approve_pending` would skip it); `staying_cards` counts it for the prompt instead.
            .filter(|p| !p.provider_prompt.as_ref().is_some_and(|prompt| prompt.needs_a_human()))
            .map(|p| (p.seq, p.permission_id.clone()))
            .collect()
    };
    ordered.sort_by_key(|(seq, _)| *seq);
    ordered.into_iter().map(|(_, id)| id).collect()
}

/// The delivered cards a bypass entry leaves waiting (review #6): still pending, not host-answered,
/// and only a human answers them (`ProviderPrompt::needs_a_human`). `waiting_cards`' complement among
/// the tab's cards, counted so the prompt can say they stay.
fn staying_cards(tab: &Tab) -> usize {
    let Some(backend) = tab.live() else {
        return 0;
    };
    let cards: BTreeSet<String> = tab.attention.card_ids().into_iter().collect();
    let projection = backend.projection();
    projection
        .pending_permissions
        .values()
        .filter(|p| cards.contains(&p.permission_id) && !tab.host_answered.contains(&p.permission_id))
        .filter(|p| p.provider_prompt.as_ref().is_some_and(|prompt| prompt.needs_a_human()))
        .count()
}

/// A bypass entry's prompt lines for a tab: R06's own line, then one saying which cards stay, when
/// any do (review #6; tmux `confirm-before` plus a consequence line, as `close_prompt` does).
fn bypass_lines(scope: tabs::PromptScope, tab: &Tab, approving: usize) -> Vec<String> {
    let mut lines = vec![tabs::bypass_prompt(scope, approving)];
    let staying = staying_cards(tab);
    if staying > 0 {
        lines.push(tabs::bypass_staying_line(staying));
    }
    lines
}

/// v1 polish F18's twin for the CLI's own prompts (review item 7): a candidate whose call completes
/// in this batch becomes its row's note, kept in `notes` for later snapshots and returned as this
/// batch's own.
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
        // The CLI's own prompts are never offered a rule (O3 ruling 3): no saved rule answers one,
        // and the CLI itself does not let a rule silence its check.
        // A card the user already answered is never offered a rule again (`user_answered`'s doc).
        let mut ids: Vec<_> = projection
            .pending_permissions
            .values()
            .filter(|p| p.provider_prompt.is_none() && !tab.user_answered.contains(&p.permission_id))
            .map(|p| (p.permission_id.clone(), p.tool_name.clone(), p.input.clone()))
            .collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        ids
    };
    let same = pending.len() == tab.rule_offers_seen.len()
        && pending.iter().zip(&tab.rule_offers_seen).all(|(p, seen)| &p.0 == seen);
    if same {
        return false;
    }
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
fn observe_turn_clock(clock: &mut Option<(String, u64)>, active_turn_id: Option<&str>, now_ms: u64) {
    match active_turn_id {
        None => *clock = None,
        Some(id) if clock.as_ref().is_some_and(|(seen, _)| seen == id) => {}
        Some(id) => *clock = Some((id.to_string(), now_ms)),
    }
}

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
        let conversation = AgentConversation::create(provider.clone(), dir).unwrap();
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

    /// A `Write` that still needs a human, on purpose: `.git/main.rs` is a protected path (fix round
    /// 1, 2026-09-28, v1 trial item 4A -- an ordinary in-project `Write` like plain `main.rs` no
    /// longer cards at all under the acceptEdits fast path, and every test below that uses this
    /// helper as a stand-in for "some permission request that needs asking" would otherwise be
    /// asserting on a card that never arrives). What is under test here is the queueing, attention
    /// and confirmation machinery, not the fast path itself -- that has its own unit tests in
    /// `agent::permission_policy` -- so any reliably-carding call would do; `.git/` keeps this one
    /// recognizably a `Write`.
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
        let two = set.open();
        let three = set.open();
        assert_eq!(set.active(), three, "tmux's new-window selects the new window");
        assert_eq!(set.get(three).unwrap().number, 3);
        set.remove(two).unwrap();
        let again = set.open();
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
        let two = set.open();
        let three = set.open();
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

    /// Spec §3.10: the pump calls `take_ui_delivery` on every tab, so the policy answers a
    /// background tab's `Read` while nothing of that tab is dispatched.
    #[test]
    fn the_pump_answers_what_needs_no_human_in_a_background_tab() {
        let dir = workspace("tabs-background-pump");
        let mut set = set();
        let background = set.active();
        let (background_provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        let foreground = set.open();
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
        // The projection still holds both until the Read's resolution returns (`answer_what_needs_no_human`'s
        // remaining honest gap), but D9/`host_answered` closes the OTHER half that used to be
        // documented here: the Read Eitri already answered is now hidden from the snapshot too,
        // not only from the tray -- only the Write, still genuinely waiting, is shown.
        assert_eq!(
            snapshot["state"]["pendingPermissions"].as_array().unwrap().len(),
            1,
            "the auto-answered Read is hidden (D9); only the Write is a real card"
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

    /// One P1-A2 run: the snapshot a switch or a reload sent (`active_state_payloads`), then the
    /// active payload of each of the two ticks after it.
    struct P1a2Run {
        snapshot: String,
        next: Option<String>,
        after: Option<String>,
    }

    /// The audit's own shape (Codex audit P1-A2, CONFIRMED): the sidecar's ingestion thread folds on
    /// its own thread, so a turn's text folds with NO tick in between -- the real gap between a tick
    /// and a switch or reload -- and the switch/reload snapshot carries it while the tab's queue
    /// still holds it. `redeliver` turns the round-2 filter off (`TabSet::redeliver_covered`).
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

    /// Round 2's shape, driven by the seam: a tick has delivered the turn's first text, `<between>`
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

    /// Codex audit P1-A2 (CONFIRMED): a switch or reload taken mid-stream was followed by a tick
    /// delivering the snapshot's own text again, and the panel rendered the reply twice. The
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

    /// P1-A2 round 2 (review, blocking): a drain and a later snapshot read are separate lock
    /// acquisitions, and an event folded between them was in the snapshot AND still queued, so the
    /// next tick delivered it again. Driven by the seam, not by timing. Also the no-loss direction:
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

    /// Both shapes with the round-2 filter off -- the semantics before it -- repeat the snapshot's
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

    /// Round 1 (Codex, CONFIRMED, blocking), found against the catch-up drain this branch then had:
    /// a `TurnCompleted` a switch or reload took off the queue never reached
    /// `PumpOutput::turn_ended` -- the only trigger `shell::agent_panel`'s ruling-3a flush has -- so
    /// a message queued behind the turn sat there until something else poked the tab. That drain is
    /// gone (round 2's review); what this holds now is that a `TurnCompleted` the snapshot carried,
    /// and the next tick therefore leaves out of its payload, is still folded by that tick into
    /// `turn_ended`, exactly once. The completion folds with NO tick in between, as in the audit.
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

        set.queue_message(tab, "follow-up", "follow-up-wire".into(), 0)
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

    /// P1-A2 round 2 (review): `pump`'s own `Resync` arm has the same shape -- the drain returns
    /// `Resync`, and the snapshot it sends is read after further lock cycles (and a bypass sweep) --
    /// so a fold in between was in that snapshot and queued for the next tick. Driven the same way:
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

    /// P1-A2 round 4 (review): the `Resync` arm's own early turn-clock read -- taken right after the
    /// drain, before this arm's further lock cycles (its three `projection()` reads and
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

    /// P1-A2 round 2: a snapshot's revision belongs to its session. A tab whose session is replaced
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

    /// P1-A2 round 2's review, minor 3: a snapshot's watermark belongs to the session it was read
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

        // As `shell::agent_panel` retires a session whose command failed fatally.
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

    /// P1-A2 round 2's review, minor 2: `shell` reports the tray's attention around the tick and a
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

    /// Review focus 2 (spec §3.9): a connect that finishes after its tab was closed is never
    /// installed anywhere; the closed tab's receiver hands the backend back for shutdown.
    #[test]
    fn a_late_start_for_a_closed_tab_is_returned_for_shutdown() {
        let dir = workspace("tabs-late-start");
        let mut set = set();
        let keep = set.active();
        let doomed = set.open();
        let (tx, result_rx) = mpsc::channel();
        set.get_mut(doomed).unwrap().backend = TabBackend::Starting(PendingStart {
            request_id: "req-1".into(),
            result_rx,
            first_turn: None,
            resume: None,
            resumed_title: None,
            resumed_name: None,
        });
        assert_eq!(set.running_count(), 1, "ruling 15: a connect counts as running");
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

    /// The whole-branch review: a resumed record with both a rename and a title labels its tab by
    /// the rename (spec §3.2), and the rename is the tab's own `name` (the detail popover, `prefix ,`).
    /// A rename given while it connected is newer, and wins.
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

        let second = set.open();
        let tx = starting(&mut set, second, Some("fix the parser"), Some("parser work"));
        set.rename(second, "mine");
        tx.send(Ok(live(&dir).1)).unwrap();
        set.collect_starts();
        assert_eq!(set.get(second).unwrap().name.as_deref(), Some("mine"));
        shut_down_all(&mut set);
    }

    /// The whole-branch review: `react` acts only on `arrived` growing, so the window's total must
    /// not drop when a tab installs a new session, resets, or closes -- or a card arriving in
    /// another tab in the same tick would not raise it.
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

        let second = set.open();
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
        let second = set.open();
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
        let other = set.open();

        assert_eq!(
            set.route_resume(other, "claude-open", true),
            ResumeRoute::SwitchTo(holder)
        );
        assert_eq!(set.active(), holder, "switched, nothing started");
        assert_eq!(
            set.route_resume(holder, "claude-elsewhere", true),
            ResumeRoute::Refuse("open in another window".to_string())
        );
        assert_eq!(
            set.route_resume(other, "claude-free", false),
            ResumeRoute::StartIn(other),
            "an empty tab takes it"
        );
        let before = set.tabs().len();
        let ResumeRoute::StartIn(new_tab) = set.route_resume(holder, "claude-free", false) else {
            panic!("a busy tab resumes into a new one")
        };
        assert_ne!(new_tab, holder);
        assert_eq!(set.tabs().len(), before + 1);
        shut_down_all(&mut set);
    }

    /// **Correction (R07/S2, D6/§2.1):** this test's old name and its "ruling 5: fixed once the
    /// session exists" assertion are both superseded -- a live, active auto tab now gets `Confirm`
    /// too, never a flat refusal. What "before and after the start" now means is that BOTH still ask
    /// before entering bypass; only leaving it is immediate, in either state (D6).
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

        // Before the session starts (NotStarted, auto): entering bypass still asks (D2), and
        // confirming also moves the window default (ruling 5, D13), so a fresh tab inherits it
        // unasked.
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
        let next = set.open();
        assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Bypass);

        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        // D6: leaving bypass works even though `next`, not `tab`, is on screen -- unlike entering
        // it, leaving never needs the tab to be active.
        assert_eq!(
            set.cycle_mode(tab),
            Ok(ModeCycle::Changed(SessionModeChoice::Auto)),
            "leaving bypass on a live tab moves at once"
        );
        // Entering it again still asks, exactly as it did before the session existed -- once `tab`
        // is the one on screen again (D2's active-tab check, unaffected by this correction).
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
            other => panic!("auto -> bypass always asks first (D2): {other:?}"),
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
            ModeCycle::Changed(SessionModeChoice::Auto),
            "leaving bypass moves at once"
        );
        shut_down_all(&mut set);
    }

    // ---- R07/S2: entering bypass asks first, and Eitri answers everything once in it --------

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
        let second = set.open();
        let (second_provider, backend) = live(&dir);
        set.get_mut(second).unwrap().backend = TabBackend::Live(backend);

        first_provider.queue(write("p1"));
        second_provider.queue(write("p2"));
        until("both cards", || {
            set.pump(&dir, true);
            set.get(first).unwrap().attention.attention().pending == 1
                && set.get(second).unwrap().attention.attention().pending == 1
        });

        // The brief's own bug: `open()` above left `second` active, and entering bypass on a tab
        // that is not on screen is refused -- select back to `first` first.
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

        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));

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

        let next = set.open();
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
            ModeCycle::Changed(SessionModeChoice::Auto),
            "bypass -> auto never asks"
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

        // A STARTING tab asks like a live one (D6): `Confirm`, LiveTab text, 0 cards. It then fails
        // to start, and the FAILED bypass tab still leaves at once.
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
                Ok(ModeCycle::Changed(SessionModeChoice::Auto)),
                "a failed bypass tab still leaves at once"
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
                Ok(ModeCycle::Changed(SessionModeChoice::Auto)),
                "an ended bypass tab still leaves at once"
            );
            shut_down_all(&mut set);
        }

        // ...and `reset` on a still-bypass ended tab keeps bypass and leaves the default untouched
        // (spec §3.1's `r` row).
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
                "reset keeps bypass (spec §3.1 r row, D6)"
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

        until("all three pending in the projection", || {
            let Some(backend) = set.get(tab).unwrap().live() else {
                return false;
            };
            backend.projection().pending_permissions.len() == 3
        });

        let out = set.pump(&dir, true);
        let payload = out.active_payload.expect("the active tab's own resync payload");
        let snapshot: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(snapshot["kind"], "snapshot");

        let mut resolved: Vec<String> = provider.resolutions().into_iter().map(|(id, _)| id).collect();
        resolved.sort();
        assert_eq!(
            resolved,
            vec!["lost1".to_string(), "lost2".to_string(), "pre".to_string()]
        );
        assert_eq!(
            provider.resolutions().iter().filter(|(id, _)| id == "pre").count(),
            1,
            "pre was already host-answered before the resync -- never answered twice"
        );

        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 0);
        let pending = snapshot["state"]["pendingPermissions"].as_array().unwrap();
        assert!(pending.is_empty(), "no swept id is drawn: {pending:?}");
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

    /// The brief's own version of this test cannot fail (a raw `PermissionModeChanged` event, which
    /// a real provider never emits with an unrestricted mode -- that becomes `UngatedCliMode`
    /// instead): what it actually pins is that `Tab::mode` is the only authority, whatever a
    /// provider reports (spec §2.2).
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

        let bypass_tab = set.open();
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
            let second = set.open();
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
                    set.open();
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
        let second = set.open();
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

        // Stays pending in the projection until its resolution is queued (`RecordingProvider` never
        // emits one on its own) -- this holds the classifier's own answer back exactly the way the
        // brief's decision 10 describes.
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

        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)));
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Auto,
            "D13: leaving bypass on ANY tab drops an in-window default too"
        );

        let next = set.open();
        assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Auto);
        shut_down_all(&mut set);
    }

    /// The whole-branch review (gate, important): a live tab's prompt (`Switch to bypass? (y/n)`, which
    /// says nothing about new sessions) answered after a terminal handoff turned the tab into
    /// `NotStarted` used to move the WINDOW default into bypass too -- every later `prefix c` then
    /// opened in bypass with no `y` for it (spec §7.2). The plan now records which question it asked,
    /// and a `y` to a question that no longer describes the tab is a reprompt, never an entry.
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
        let next = set.open();
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

    /// The whole-branch review (gate, minor): D3's "a launch never starts in bypass" held only because
    /// the one caller passes `agent_prefs::startup_mode`, which never returns it. Now it holds here.
    #[test]
    fn a_window_never_starts_in_bypass_whatever_it_is_handed() {
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Bypass);
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert_eq!(set.active_tab().mode(), SessionModeChoice::Auto);
        let next = set.open();
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
        let second = set.open();
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
        let second = set.open();
        let third = set.open();
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
        let conversation = AgentConversation::create(provider.clone(), dir).unwrap();
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
            set.queue_message(tab, "and the tests", "ctx A\n\nand the tests".into(), 1),
            Ok(1)
        );
        assert_eq!(
            set.queue_message(tab, "then commit", "ctx B\n\nthen commit".into(), 2),
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

    /// Wave 4 R1: vim's `:bd` asks only when something would be lost (E89); so does `<leader>bd`.
    #[test]
    fn closing_asks_only_for_a_running_turn_a_connect_or_a_queue() {
        let dir = workspace("tabs-close-needs-confirm");
        let mut set = set();
        let empty = set.active();
        assert!(!set.close_needs_confirm(empty), "an empty tab has nothing to lose");
        assert!(!set.close_needs_confirm(TabId(999)), "no such tab");

        let connecting = set.open();
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
            "a connect in flight (ruling 15 counts it as running)"
        );

        let (provider, backend) = live(&dir);
        set.get_mut(empty).unwrap().backend = TabBackend::Live(backend);
        assert!(!set.close_needs_confirm(empty), "an idle live tab: nothing to lose");
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, empty, true);
        assert!(set.close_needs_confirm(empty), "a running turn");
        assert_eq!(set.queue_message(empty, "later", "later".into(), 1), Ok(1));
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

        let other = set.open();
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

    /// Review focus 1 (spec §4.4): a flush the backend refuses stays in the queue with a line saying
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
        set.queue_message(tab, "later", "later".into(), 1).unwrap();
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

    /// §4.1: "A pending card holds the queue."
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
        set.queue_message(tab, "after the card", "after the card".into(), 1)
            .unwrap();
        assert!(set.flush_queue(tab).is_none());
        assert!(provider.turns().is_empty());
        shut_down_all(&mut set);
    }

    /// D5 A: after an interrupt the queue still goes out, with what `Ctrl+Enter` added.
    #[test]
    fn send_now_interrupts_and_the_queue_goes_out_when_the_turn_ends() {
        let dir = workspace("tabs-send-now");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = interruptible_live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(started("t1"));
        pump_until_running(&mut set, &dir, tab, true);
        set.queue_message(tab, "one", "one".into(), 1).unwrap();
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
        set.queue_message(tab, "a", "a".into(), 1).unwrap();
        set.queue_message(tab, "b", "b".into(), 2).unwrap();
        assert_eq!(set.take_back_queue(tab), vec!["a".to_string(), "b".to_string()]);
        assert!(set.get(tab).unwrap().queue.is_empty());
        assert!(set.take_back_queue(tab).is_empty());
        shut_down_all(&mut set);
    }

    /// Ruling 5.
    #[test]
    fn queueing_needs_a_session_that_is_starting_or_alive() {
        let mut set = set();
        let tab = set.active();
        assert!(
            set.queue_message(tab, "x", "x".into(), 1).is_err(),
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
            set.queue_message(tab, "x", "x".into(), 1),
            Ok(1),
            "the box is live while it starts"
        );
        set.get_mut(tab).unwrap().backend = TabBackend::Failed { reason: "no".into() };
        assert!(set.queue_message(tab, "y", "y".into(), 2).is_err());
    }

    /// Ruling 9 (spec §4.4): `r` on an ended tab puts its queue back into the draft.
    #[test]
    fn reset_folds_the_queue_and_the_old_draft_into_the_draft() {
        let mut set = set();
        let tab = set.active();
        {
            let t = set.get_mut(tab).unwrap();
            t.queue.push(Queued {
                text: "q1".into(),
                wire: "q1".into(),
                queued_at_ms: 1,
            });
            t.queue.push(Queued {
                text: "q2".into(),
                wire: "q2".into(),
                queued_at_ms: 2,
            });
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
        let two = set.open();
        for (tab, text) in [(one, "a"), (two, "b"), (two, "c")] {
            set.get_mut(tab).unwrap().queue.push(Queued {
                text: text.into(),
                wire: text.into(),
                queued_at_ms: 0,
            });
        }
        assert_eq!(set.close_facts(two).unwrap().queued, 2);
        assert_eq!(set.queued_count(), 3);
        assert_eq!(
            set.take_every_queued_text(),
            vec!["a".to_string(), "b".into(), "c".into()]
        );
        assert_eq!(set.queued_count(), 0);
    }

    /// D7 through the tab set: the rules answer a background tab's call, and a card that a rule
    /// could answer is offered one.
    #[test]
    fn rules_answer_a_background_call_and_a_card_is_offered_the_rule_that_would() {
        let dir = workspace("tabs-rules");
        let mut set = set();
        set.set_rules(agent::PrefixRules::default().with(agent::PrefixRule::parse("Bash(npm ci *)").unwrap()));
        let background = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(background).unwrap().backend = TabBackend::Live(backend);
        set.open();
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

    /// v1 polish F18: a call a saved rule answered is named in the events payload and in every later
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

    /// v1 trial item 7: an in-project `Write` the acceptEdits fast path answers with no card is
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

    /// Fix round finding 1: a `Write` the acceptEdits fast path answers with no card still marks
    /// its completed row as creating a file, when the target did not exist -- F22's own
    /// `note_new_files` never runs for this call, since it is driven by a delivered
    /// `PermissionRequested`, which `answer_what_needs_no_human` drops. Before this, the row kept
    /// the overwrite warning for a file that never existed. Since whole-branch review finding 6 the
    /// check is made by `answer_what_needs_no_human` itself, just before its `allow` is sent (it was
    /// a `ToolCallStarted`-driven `note_auto_creates_file`, gated to Auto).
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
        // The gate the fast path answers (whole-branch review finding 2: the note is recorded where
        // that answer happens, so a test with no gate has nothing to note -- this one used to leave
        // it out, the very shape of a call that failed validation before the CLI asked).
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

    /// Codex's finding (fix round finding 2), and this task's own correction (v1 trial whole-branch
    /// review, fix round 3): the `Resync` arm's still-pending sweep protects a genuine fast-path
    /// candidate -- a queue overflow can drop a follow-up card's own `PermissionRequested` (the event
    /// that would ordinarily remove it from `auto_edit_candidates`) while the projection (complete by
    /// construction) still shows the permission pending, so the `Resync` snapshot draws a real card
    /// for it. Left alone, the human's later approval on that card would complete the call through
    /// the ordinary path and wrongly read "allowed by auto" for a call answered on screen.
    ///
    /// **Correction (this task):** the previous version of this test edited `.git/probe`, which item
    /// 4A (2026-09-28) cards outright -- no fast-path candidate was ever created, so the assertions
    /// below passed even with the still-pending sweep's `retain` deleted, pinning nothing. This
    /// version edits an in-project file the fast path genuinely answers, then overflows the CLI's own
    /// follow-up prompt for the same call so a real candidate survives to be swept.
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

    /// Whole-branch review finding 2 (v1 trial, 2026-09-28), the helpers for the three shapes below.
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

    /// Whole-branch review finding 2: the CLI runs a tool's `validateInput` BEFORE its `PreToolUse`
    /// hooks, so an `Edit` of a file not read yet, or whose `old_string` is not there, completes
    /// with `is_error` and no gate call at all. Nothing answered it -- the note used to be inferred
    /// from the missing card, so an `Edit` of `/etc/hosts` that failed validation read "allowed by
    /// auto".
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

    /// Whole-branch review finding 2: a card whose request names no tool-use id (the wire's empty id
    /// maps to `None`) could never drop the candidate its `ToolCallStarted` made, so the human's own
    /// approval of an out-of-project `Write` completed it as "allowed by auto".
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

    /// Whole-branch review finding 2: a switch to bypass between the call's start and its gate means
    /// bypass answered it (`answer_what_needs_no_human`'s bypass branch), never the fast path -- the
    /// candidate made in Auto used to survive into a note anyway.
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

    /// Whole-branch review finding 6: bypass answers a `Write`'s gate and drops the request, so
    /// F22's `note_new_files` (driven by a delivered `PermissionRequested`) never saw it, and the
    /// fast path's own note was gated to Auto -- a bypass `Write` that created a file read "Writes the
    /// whole file. The request does not say what is there now." The check now runs where the answer
    /// is given, before it is sent, in both modes; a file that exists gets no such note.
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

    /// Finding 6's note must reach the panel on the events path too, not only in a later snapshot.
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

    /// Whole-branch review finding 2, the other half of recording the note where the answer is: a
    /// fast-path answer is a candidate until its call completes, and a queue overflow that drops the
    /// completion must still turn it into its note. **Correction (v1 trial fix round 2):** this test
    /// lets the call finish before the `Resync`, so the finished sweep consumes the candidate before
    /// the still-pending sweep's `host_answered` filter is reached; that filter is pinned by
    /// `a_resync_before_the_call_finishes_keeps_its_fast_path_answer` below.
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

    /// v1 trial fix round 2 (re-review of finding 2, part a): a `Resync` that lands after the fast
    /// path answered an edit's gate but BEFORE its call completes. The provider has not reported the
    /// request resolved, so the projection still lists it pending; the still-pending sweep must leave
    /// it out (`host_answered`) and keep the candidate, or the finished call would lose its "allowed
    /// by auto" note to a `Resync` that merely came early.
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

    /// v1 trial fix round 2 (re-review of finding 2, part b): the fast path allowed the gate, then the
    /// CLI raised its own prompt for the same call (its sensitive-file check, O3), a human approved
    /// that card, and the call completed. A human answered it, so it must not read "allowed by auto"
    /// -- the `PermissionRequested` arm of `note_auto_edit_answers` drops the candidate.
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

    /// v1 trial fix round 2 (re-review of finding 2, part c): only `Write`/`Edit`/`NotebookEdit`
    /// (`ACCEPT_EDITS_TOOLS`) answered by the fast path become candidates. A `Read` the classifier
    /// allowed is answered by the same `answer_what_needs_no_human` and must not read "allowed by
    /// auto" -- auto's note names the acceptEdits fast path (item 4A), not the read-only rules
    /// every mode already had.
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

    /// The item-7 review's finding 2 sibling gap (dated record, 2026-09-28, v1 trial, item 7), and
    /// this task's own fix (whole-branch review, fix round 3): the `Resync` arm's still-pending
    /// sweep protects `rule_candidates` the same way it protects `auto_edit_candidates` -- a queue
    /// overflow can drop a follow-up card's own `PermissionRequested` (the event that would
    /// ordinarily remove the candidate) while the projection (complete by construction) still shows
    /// the permission pending, so the `Resync` snapshot draws a real card for it. Left alone, the
    /// human's later approval on that card would complete the call through the ordinary path and
    /// wrongly read "allowed by rule" for a call answered on screen.
    ///
    /// **Correction (this task):** the setup used to create the candidate from a bare
    /// `ToolCallStarted`, which is exactly the pre-existing bug this task fixes -- `rule_candidates`
    /// is now populated only from an answer `AgentBackend::answer_what_needs_no_human` actually gave.
    /// The gate is answered by the rule first (a real candidate, recorded at that answer), then the
    /// CLI's own follow-up prompt for the same call is the one whose overflow-dropped
    /// `PermissionRequested` this test pins.
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

    /// P1-A2 round 2: when a snapshot already carried every event of the next drain, the payload
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

    /// v1 polish F22: a `Write` card whose file is not there when it is raised says it creates one,
    /// in the events payload and in later snapshots (on the card and on its call); a card over an
    /// existing file does not, nor does its file appearing later change what the card said.
    #[test]
    fn a_write_card_over_no_file_is_marked_as_creating_one() {
        // Fix round 1 (2026-09-28, v1 trial item 4A): both targets are now under `.git/` -- an
        // ordinary top-level `new.txt`/`old.txt` no longer cards at all under the acceptEdits fast
        // path, so this test (which is about the card's `createsFile` flag, not about the fast path
        // itself) needs a target that still reaches a card either way.
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

    /// v1 polish F22, the local review: only "not found" is no file. A path this process cannot
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

    /// v1 polish F18, the local review, rewritten for this task's fix (whole-branch review, fix
    /// round 3): `note_rule_answers` no longer creates its own candidates from a `ToolCallStarted`'s
    /// arguments (that was the pre-existing bug), so this seeds `candidates` directly, the way `pump`
    /// extends `tab.rule_candidates` from `AnsweredForYou::by_rule`. A card kept for a candidate (the
    /// rule's own auto-answer failed, or the CLI's own follow-up prompt after the gate) drops the
    /// note; a candidate with no such card becomes one once its call completes.
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

    /// Whole-branch review (v1 trial, fix round 3): the pre-existing F18 rule note had the same
    /// defect class as finding 2 -- `note_rule_answers` used to re-ask `agent::rule_that_allows` on a
    /// `ToolCallStarted`'s own arguments and infer the answer from no card ever having arrived. The
    /// CLI runs a tool's `validateInput` BEFORE its `PreToolUse` hooks, so a `Bash` call that fails
    /// validation completes with `is_error` and no `PermissionRequested` -- and no gate at all for a
    /// rule to have answered -- but used to read "allowed by rule npm ci *" all the same.
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

    /// Whole-branch review (v1 trial, fix round 3): a switch to bypass between the call's start and
    /// its gate means bypass answered it (`answer_what_needs_no_human`'s bypass branch, which never
    /// classifies), never the rule -- the candidate the old speculative check made from the started
    /// call's own arguments used to survive into a note anyway.
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

    /// Review item 2: the CLI's own follow-up prompt for a call a rule already answered can carry
    /// an EMPTY tool-use id (Verdandi's `permissionBroker.ts` sends `toolUseId: ''` when the CLI
    /// gave none; `translate.rs` maps that to `None`), so the ordinary "same id" removal in
    /// `note_rule_answers` can never find the candidate it should drop -- before this fix, the row
    /// still read "allowed by rule" once a human approved that id-less card and the call completed.
    /// Matched by content instead (`dropped_candidate_id`).
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

        // The CLI's own follow-up prompt for the same call, with NO tool-use id at all: the shape
        // this review item is about. `needs_a_human()` is false (no ask rule, no unrecognized
        // origin), and in Auto `HumanApprovals::matches` never matches an id-less prompt ("a prompt
        // with no id matches nothing"), so this becomes a real card rather than being silently
        // re-answered by the rule a second time.
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

    /// Missing test (this task's own review, item 3): a rule-eligible call whose auto-answer the
    /// provider refuses stays a real card (`answer_what_needs_no_human`'s "fail toward a card" doc,
    /// mirroring `a_failed_allow_in_bypass_draws_the_card` in `agent_backend`'s own tests) -- with no
    /// `by_rule` candidate ever recorded, since that is only ever extended in the `Ok` arm. The gate
    /// itself carries an empty tool-use id, the shape review item 2 is about, to prove the new
    /// content match in `note_rule_answers` (`dropped_candidate_id`) finds nothing to drop here and
    /// stays inert: a human's own approval of this card must never read "allowed by rule".
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

    /// Whole-branch review (v1 trial, fix round 3): the sibling gap `a_resync_turns_a_fast_path_
    /// answer_whose_call_finished_into_its_note` fixed for `auto_edit_candidates` never had a rule
    /// counterpart -- the `Resync` arm's "finished" sweep only ever covered `prompt_note_candidates`
    /// and `auto_edit_candidates`, so a rule candidate whose call finished among a queue overflow's
    /// dropped events lingered as a live candidate for good and never became its note at all.
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
        set.open();
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
                    .is_some_and(|t| t.is_finished())
        });
        let trace = set.get(background).unwrap().turn_trace.as_ref().unwrap();
        assert!(trace.line().contains("first_paint_frame=bg"), "{}", trace.line());
        assert!(
            trace.is_emitted(),
            "the pump emits a background trace: nothing else will"
        );
        shut_down_all(&mut set);
    }

    /// Nothing previously pinned how `pump` feeds a drained batch into the turn trace. A turn that
    /// folds entirely before the very first tick sees it (P1-A2's own shape) is delivered to the
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
        set.get_mut(tab).unwrap().turn_trace = Some(crate::turn_trace::TurnTrace::started_now());

        // The whole turn folds with no tick draining it -- the ingestion thread folds on its own,
        // exactly as the P1-A2 races do.
        provider.queue(started("t1"));
        provider.queue(text_delta("t1", "hi"));
        provider.queue(completed("t1", agent::TurnOutcome::Completed));
        wait_for_fold(&set, tab, "hi");

        // Reads the snapshot and notes its revision on the backend -- the watermark the next
        // pump's `covered` filter reads (P1-A2 round 2).
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
        set.queue_message(first, "queued", "queued".into(), 5).unwrap();
        set.set_draft(first, "typing");
        let second = set.open();
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

    /// Wave-4 GUI pass: a first message sent from an empty tab left its mirrored draft here, and the
    /// state payloads sent once the connect finished put the sent text back into the new
    /// conversation's composer.
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

    /// Review focus 4: a draft edited in nvim returns to the tab it came from, after a switch too.
    #[test]
    fn a_scratch_edit_returns_to_its_own_tab() {
        let mut set = set();
        let first = set.active();
        set.set_draft(first, "before");
        set.begin_scratch_edit(first, 7).unwrap();
        assert!(set.begin_scratch_edit(first, 8).is_err(), "one edit per tab at a time");
        let second = set.open();
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

    // ---- R07, D12: a CLI reporting an ungated mode closes the session ----

    /// D12 at the one place that closes a session: an `UngatedCliMode` fails the tab with a reason
    /// naming the setting and the mode, hands the backend out for the shutdown path a closed tab
    /// uses, and delivers nothing of the batch it came in -- so the `Write` queued behind it is
    /// never a card anyone could approve, and the shutdown is what denies it.
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

    /// `Tab::mode` is the only authority (spec §2.2): a `PermissionModeChanged` that reached the
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

    /// O3 rulings 5 and 7, the whole sequence the real CLI produces in Auto: the gate's card, the
    /// human's Approve on it (`answer_card`), the gate's resolution, then the CLI's own prompt for the
    /// SAME call under a new permission id. That second request is accepted (ruling 7) and answered
    /// `allow` without a second card (ruling 5): exactly one card for the call.
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
        });
        until("the turn ends", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().human_allowed.is_empty()
        });
        shut_down_all(&mut set);
    }

    /// O3 ruling 4 through the tab set: in bypass the CLI's own prompt is answered and never drawn.
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

    /// O3 ruling 3, the card half: a saved rule can never silence the CLI's own prompt, so its card
    /// never offers "Always allow" -- while the gate's card for the same command still does.
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

    /// O3 ruling 4's exception at a bypass entry: a card the user's own ask rule forced is not among
    /// the cards `y` approves -- it is not counted, not listed, and still waiting afterwards.
    #[test]
    fn entering_bypass_leaves_a_card_the_users_ask_rule_forced() {
        let dir = workspace("o3-bypass-entry-ask");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.queue(cli_prompt(
            "perm-ask",
            "toolu_g",
            "Bash",
            serde_json::json!({ "command": "cat notes.txt" }),
            Some("cat:*"),
        ));
        provider.queue(write("perm-write"));
        until("both cards", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().attention.attention().pending == 2
        });
        let plan = plan(set.cycle_mode(tab));
        assert_eq!(plan.approve, vec!["perm-write".to_string()]);
        // Review #6: the prompt says the card that will stay, on a line of its own after R06's.
        assert_eq!(plan.lines.len(), 2, "{:?}", plan.lines);
        assert!(plan.lines[0].contains("approve the 1 waiting card"), "{:?}", plan.lines);
        assert!(
            plan.lines[1].contains("1 card") && plan.lines[1].contains("stays waiting"),
            "{:?}",
            plan.lines
        );
        assert_eq!(
            set.confirm_bypass(plan.scope, plan.nonce),
            Ok(ConfirmOutcome::Entered {
                approved: 1,
                resolved: Vec::new()
            })
        );
        assert_eq!(provider.resolutions(), vec![("perm-write".to_string(), true)]);
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 1);
        assert!(set
            .get(tab)
            .unwrap()
            .live()
            .unwrap()
            .projection()
            .pending_permissions
            .contains_key("perm-ask"));
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

    /// Review item 7 (the spike's row note): a call whose CLI prompt was answered without a card says
    /// so, muted, on its row -- in the events payload when the call completes, and in every later
    /// snapshot -- as a call a saved rule answered does.
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
            serde_json::json!([{ "toolUseId": "toolu_n", "note": "Claude Code safety check — allowed in bypass" }])
        );
        let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
        assert_eq!(
            snapshot["state"]["toolCalls"][0]["promptNote"],
            "Claude Code safety check — allowed in bypass"
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
        let empty = set.open();
        let third = set.open();
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
        let b = set.open();
        give_session(&mut set, b, &dir, "s-b");
        let c = set.open(); // empty
        let d = set.open(); // empty
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
        let failed = set.open();
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
        set.open();
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
        let next = set.open();
        assert_eq!(
            set.get(next).unwrap().mode(),
            SessionModeChoice::Bypass,
            "and every new one"
        );
        // Leaving bypass is as it always was: at once, and the default goes with it.
        let tab = set.active();
        assert_eq!(set.cycle_mode(tab), Ok(ModeCycle::Changed(SessionModeChoice::Auto)),);
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
    }

    fn planned(id: &str, mode: SessionModeChoice) -> PlannedTab {
        PlannedTab {
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
        set.get(set.tab_with_session(session).expect("the session has a tab"))
            .unwrap()
            .mode()
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
        set.start_restore(from, go, |id| {
            asked.push(id.to_string());
            Ok(mpsc::channel().1)
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
        set.start_restore(from, go, |_| Ok(mpsc::channel().1));
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
        let other = set.open();
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
        set.start_restore(from, kept, |_| Ok(mpsc::channel().1));
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
        let run = set.start_restore(from, go, |_| Ok(mpsc::channel().1));
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
        set.start_restore(from, go, |_| Ok(mpsc::channel().1));
        assert_eq!(set.active_tab().provider_session_id().as_deref(), Some("a"));
    }

    #[test]
    fn a_session_already_open_here_is_switched_to_and_never_resumed_twice() {
        let dir = workspace("tabs-restore-dedupe");
        let mut set = set();
        let holder = set.active();
        give_session(&mut set, holder, &dir, "s-open");
        let from = set.open();
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
        let run = set.start_restore(from, go, |id| {
            connected.push(id.to_string());
            Ok(mpsc::channel().1)
        });
        assert_eq!(connected, ["s-new"], "the open one is not connected again");
        assert_eq!(set.tab_with_session("s-open"), Some(holder));
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
        let run = set.start_restore(from, go, |id| {
            if id == "held" {
                Err("open in another window".to_string())
            } else {
                Ok(mpsc::channel().1)
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
        let mut run = set.start_restore(from, go, |_| {
            let (tx, rx) = mpsc::channel();
            senders.push(tx);
            Ok(rx)
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
        assert!(set.tab_with_session("b").is_none());

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
        let mut run = set.start_restore(from, go, |_| Ok(rx.take().unwrap()));
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
        set.discard_failed_restore(tab, Some(SessionModeChoice::Auto));
        let other = set.open();
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

        let empty = set.open();
        assert!(
            !show(&set, &mut memory),
            "an empty tab is not listed, and neither is its being active"
        );
        let second = set.open();
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
}
