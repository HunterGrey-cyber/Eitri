//! One window's session tabs (session tabs spec §3.1): one conversation, and so one `AgentBackend`,
//! per tab, every one of them running. GTK-free: `shell::agent_panel` spawns the connect and
//! handoff workers and hands this set their receivers; the 33 ms tick calls [`TabSet::pump`], which
//! drains EVERY tab -- `take_ui_delivery` is where the permission policy answers what needs no
//! human, so a tab that was not pumped would stall on its first `Read` (spec §3.1).
//!
//! **Lock order, inherited:** on the sidecar path `projection()` holds the ingestion mutex and
//! `provider_session_id()` takes it again. Never call the second while holding the first's guard
//! (the 2026-09-15 GTK freeze). Every method below reads them in separate statements.

use std::path::Path;
use std::sync::mpsc;

use agent::{AgentDomainEvent, UiDelivery};

use crate::agent_backend::{AgentBackend, BackendError, BackendKind};
use crate::agent_bridge::{
    serialize_events_for_js, serialize_handoff_for_js, serialize_snapshot_for_js, serialize_tabs_for_js,
    SessionModeChoice, SnapshotView, TabRef, TabStateWire, TabView,
};
use crate::attention::{Attention, AttentionTracker};
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
    pub mode: SessionModeChoice,
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
            turn_clock: None,
        }
    }

    /// When this tab's running turn started, if one is running (see `turn_clock`).
    pub fn turn_started_at_ms(&self) -> Option<u64> {
        self.turn_clock.as_ref().map(|(_, at)| *at)
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
    /// The active tab's turn trace saw its first text in this batch.
    pub first_text: bool,
    /// Tabs whose turn went from running to not running in this tick: the caller flushes their
    /// queues (ruling 3a).
    pub turn_ended: Vec<TabId>,
    /// Tabs whose `rule_offers` changed in this tick: the caller sends them to the panel.
    pub offers_changed: Vec<TabId>,
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

pub struct TabSet {
    tabs: Vec<Tab>,
    active: TabId,
    last_active: Option<TabId>,
    next_id: u64,
    kind: BackendKind,
    default_mode: SessionModeChoice,
    /// Every card the tabs removed from this set had been handed, so the window's `arrived`
    /// (`attention`) never goes down when a tab closes: [`crate::attention::react`] reads only its
    /// growth.
    removed_arrived: u64,
    /// The project's D7 prefix rules, window-level: every tab's pump applies them (ruling 17).
    rules: agent::PrefixRules,
}

impl TabSet {
    /// One empty tab 1 (D10: open tabs are not restored across launches).
    pub fn new(kind: BackendKind, default_mode: SessionModeChoice) -> Self {
        let mut set = TabSet {
            tabs: Vec::new(),
            active: TabId(0),
            last_active: None,
            next_id: 1,
            kind,
            default_mode,
            removed_arrived: 0,
            rules: agent::PrefixRules::default(),
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
        }
        true
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
        let tab = self.tabs.remove(at);
        self.removed_arrived += tab.attention.attention().arrived;
        if self.last_active == Some(id) {
            self.last_active = None;
        }
        if self.tabs.is_empty() {
            self.open();
            self.last_active = None;
        } else if self.active == id {
            let fallback = self.tabs[at.min(self.tabs.len() - 1)].id;
            self.active = self.last_active.take().unwrap_or(fallback);
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

    /// `Shift+Tab` on an empty tab (ruling 5). `None` once the session exists.
    pub fn cycle_mode(&mut self, id: TabId) -> Option<SessionModeChoice> {
        let tab = self.get_mut(id)?;
        if !matches!(tab.backend, TabBackend::NotStarted) {
            return None;
        }
        tab.mode = tab
            .mode
            .cycled(crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES);
        let mode = tab.mode;
        self.default_mode = mode;
        Some(mode)
    }

    /// Shift+Tab on a started tab (D6, wave 5): the session switches between auto and bypass when its
    /// sidecar can (`set_permission_mode`), once the sidecar acknowledged -- never optimistically
    /// (W1: "a requested permission mode must never silently become a different one", `provider.rs`).
    /// Entering bypass answers the cards already waiting on THIS tab (owner, 2026-09-26: "能不能做自动
    /// 放行"; W4); Verdandi leaves them pending. Does not touch `default_mode` (W2): that is chosen on
    /// an empty tab or the chooser, and Claude Code's own Shift+Tab does not persist either. There is
    /// deliberately no reconnect or resume-in-another-mode fallback for a session that cannot switch.
    pub fn switch_mode(&mut self, id: TabId) -> Result<SessionModeChoice, String> {
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let target = tab
            .mode
            .cycled(crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES);
        let ended = matches!(tab.wire_state(), TabStateWire::Ended | TabStateWire::Failed);
        let backend = match &mut tab.backend {
            TabBackend::Live(backend) => backend,
            // W5's text, which the panel flashes itself; said here too for a panel that posts anyway.
            TabBackend::Starting(_) => return Err("session is starting — try again once it is up".into()),
            TabBackend::NotStarted | TabBackend::Failed { .. } => return Err("the session has not started yet".into()),
        };
        if !backend.can_switch_mode() {
            return Err("the mode is fixed for this session".into());
        }
        if ended {
            return Err("the session has ended".into());
        }
        backend.set_permission_mode(target.into()).map_err(|e| e.message)?;
        tab.mode = target;
        if target == SessionModeChoice::Bypass {
            backend.allow_all_pending();
        }
        Ok(target)
    }

    /// `Shift+Tab` on the chooser's `New session` row or a record, once the active tab is not itself
    /// `NotStarted` (spec §6.3, panel round 2 plan Task 5): cycles the window's remembered
    /// `default_mode` -- the mode a fresh tab, or a resume that opens a new tab, takes. Every open
    /// tab keeps whatever mode it already has; ruling 5 ("fixed once the session exists") is about a
    /// *tab's own* mode, and does not apply here since this never touches one.
    pub fn cycle_default_mode(&mut self) -> SessionModeChoice {
        self.default_mode = self
            .default_mode
            .cycled(crate::agent_backend::CLIENT_IMPLEMENTED_PERMISSION_MODES);
        self.default_mode
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
        tab.was_running = false;
        let old = std::mem::replace(&mut tab.backend, TabBackend::NotStarted);
        tab.title = None;
        tab.attention.restart();
        tab.stale = false;
        tab.turn_trace = None;
        tab.reported_start_failure = false;
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
            first_text: false,
            turn_ended: Vec::new(),
            offers_changed: Vec::new(),
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
                continue;
            };
            let from_revision = backend.projection().last_revision;
            let mut turn_ended = false;
            let delivery = backend.take_ui_delivery_with_rules(project_root, rules);
            observe_turn_clock(
                &mut tab.turn_clock,
                backend.projection().active_turn_id.as_deref(),
                wall_clock_ms(),
            );
            let turn_started_at_ms = tab.turn_clock.as_ref().map(|(_, at)| *at);
            match delivery {
                UiDelivery::Nothing => {}
                UiDelivery::Events(events) => {
                    for event in &events {
                        match event {
                            AgentDomainEvent::TurnStarted { .. } => tab.was_running = true,
                            // An interrupt's end and the legacy backend's synthesized end are
                            // `TurnCompleted`s too. Reported whatever `was_running` says: a legacy
                            // `TurnStarted` is returned by `send_turn` and never pumped.
                            AgentDomainEvent::TurnCompleted { .. } => {
                                tab.was_running = false;
                                turn_ended = true;
                            }
                            // The session ended: its queue waits for `r` (ruling 9), not a flush.
                            AgentDomainEvent::SessionClosed { .. } | AgentDomainEvent::SessionUnavailable { .. } => {
                                tab.was_running = false
                            }
                            // The sidecar's own report wins over a stale tab (wave 5). It moves the
                            // band only: auto-approval reads the acknowledged mode, never `tab.mode`.
                            AgentDomainEvent::PermissionModeChanged { mode, .. } => tab.mode = choice_of(*mode),
                            _ => {}
                        }
                    }
                    tab.attention.observe(&events, on_screen);
                    if let Some(trace) = tab.turn_trace.as_mut() {
                        let first_text = trace.observe(&events);
                        if is_active {
                            out.first_text |= first_text;
                        } else if first_text {
                            trace.mark_background();
                        }
                    }
                    if is_active {
                        let through_revision = backend.projection().last_revision;
                        out.active_payload = Some(serialize_events_for_js(
                            tab.id,
                            from_revision,
                            through_revision,
                            &events,
                        ));
                    } else {
                        tab.stale = true;
                    }
                }
                UiDelivery::Resync => {
                    // The events are gone; the snapshot is what the panel is shown now.
                    let running = backend.projection().active_turn_id.is_some();
                    turn_ended = tab.was_running && !running;
                    tab.was_running = running;
                    let pending: Vec<String> = backend.projection().pending_permissions.keys().cloned().collect();
                    tab.attention.resync(pending);
                    if is_active {
                        out.active_payload = Some(serialize_snapshot_for_js(
                            tab.id,
                            &SnapshotView::of(backend),
                            turn_started_at_ms,
                        ));
                    } else {
                        tab.stale = true;
                    }
                }
            }
            // A background trace has no dispatch to wait for: printed here when it finishes.
            if !is_active {
                if let Some(trace) = tab.turn_trace.as_mut() {
                    if trace.is_complete() {
                        trace.emit();
                    }
                }
            }
            let still: std::collections::HashSet<String> =
                backend.projection().pending_permissions.keys().cloned().collect();
            tab.attention.retain_pending(|id| still.contains(id));
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
                    tab.last_handoff = None;
                    tab.reported_start_failure = false;
                    tab.title = pending.resumed_title;
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
    pub fn active_state_payloads(&mut self) -> Vec<String> {
        let tab = self.active_tab_mut();
        tab.stale = false;
        let mut payloads = Vec::new();
        match &tab.backend {
            TabBackend::Live(backend) => payloads.push(serialize_snapshot_for_js(
                tab.id,
                &SnapshotView::of(backend),
                tab.turn_clock.as_ref().map(|(_, at)| *at),
            )),
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
        std::mem::take(&mut self.tabs)
    }
}

/// The tab's choice for a mode a provider reported. Only the reverse `From` exists
/// (`agent_bridge`), and this one is needed only by the pump.
fn choice_of(mode: agent::PermissionMode) -> SessionModeChoice {
    match mode {
        agent::PermissionMode::Auto => SessionModeChoice::Auto,
        agent::PermissionMode::Bypass => SessionModeChoice::Bypass,
    }
}

/// Recomputes `rule_offers` when the set of pending permission ids changed; `true` if the offers did.
/// Only `projection()` is read, once, and released before anything else is touched.
fn refresh_offers(tab: &mut Tab, project_root: &Path) -> bool {
    let Some(backend) = tab.live() else {
        let had = !tab.rule_offers.is_empty();
        tab.rule_offers.clear();
        tab.rule_offers_seen.clear();
        return had;
    };
    let pending: Vec<(String, String, serde_json::Value)> = {
        let projection = backend.projection();
        let mut ids: Vec<_> = projection
            .pending_permissions
            .values()
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
    use agent::{AgentConversation, AgentDomainEvent, PermissionMode};
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
        let conversation = AgentConversation::create(provider.clone(), dir, PermissionMode::Auto).unwrap();
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
        }
    }

    fn write(id: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionRequested {
            permission_id: id.into(),
            tool_use_id: None,
            tool_name: "Write".into(),
            input: serde_json::json!({ "file_path": "main.rs", "content": "" }),
        }
    }

    fn set() -> TabSet {
        TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto)
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
        assert_eq!(
            snapshot["state"]["pendingPermissions"].as_array().unwrap().len(),
            2,
            "the projection holds both until the Read's resolution returns"
        );
        assert!(!set.get(background).unwrap().stale);
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

    #[test]
    fn a_rename_is_normalized_and_the_mode_cycles_only_before_the_start() {
        let dir = workspace("tabs-rename-mode");
        let mut set = set();
        let tab = set.active();
        assert!(set.rename(tab, "  docs  "));
        assert_eq!(set.get(tab).unwrap().label_name(), "docs");
        assert!(set.rename(tab, ""));
        assert_eq!(set.get(tab).unwrap().name, None);
        assert!(!set.rename(TabId(99), "x"));

        assert_eq!(set.cycle_mode(tab), Some(SessionModeChoice::Bypass));
        assert_eq!(
            set.default_mode(),
            SessionModeChoice::Bypass,
            "new tabs take the remembered mode"
        );
        let next = set.open();
        assert_eq!(set.get(next).unwrap().mode, SessionModeChoice::Bypass);
        let (_provider, backend) = live(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.cycle_mode(tab), None, "ruling 5: fixed once the session exists");
        shut_down_all(&mut set);
    }

    #[test]
    fn cycle_default_mode_moves_the_window_default_and_leaves_open_tabs_alone() {
        let mut set = set();
        let tab = set.active();
        assert_eq!(set.default_mode(), SessionModeChoice::Auto);
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto);
        assert_eq!(set.cycle_default_mode(), SessionModeChoice::Bypass);
        assert_eq!(set.default_mode(), SessionModeChoice::Bypass);
        assert_eq!(
            set.get(tab).unwrap().mode,
            SessionModeChoice::Auto,
            "the open tab's own mode is untouched"
        );
        assert_eq!(
            set.cycle_default_mode(),
            SessionModeChoice::Auto,
            "wraps like cycle_mode does"
        );
        shut_down_all(&mut set);
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

    fn interruptible_live(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(RecordingProvider::interruptible());
        let conversation = AgentConversation::create(provider.clone(), dir, PermissionMode::Auto).unwrap();
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

    // ---- wave 5, Task 4: Shift+Tab switches a live tab; entering bypass answers its cards ----

    fn live_switchable(dir: &Path) -> (Arc<RecordingProvider>, AgentBackend) {
        let provider = Arc::new(RecordingProvider::switchable());
        let conversation = AgentConversation::create(provider.clone(), dir, PermissionMode::Auto).unwrap();
        (provider, AgentBackend::Sidecar(Box::new(conversation)))
    }

    /// Pumps until `tab` holds `pending` cards the pump delivered.
    fn pump_until_pending(set: &mut TabSet, dir: &Path, tab: TabId, pending: usize) {
        until("the pending cards", || {
            set.pump(dir, true);
            set.get(tab).unwrap().attention.attention().pending == pending
        });
    }

    #[test]
    fn a_started_tab_switches_both_ways_when_the_sidecar_can() {
        let dir = workspace("tabs-switch-both-ways");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live_switchable(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let default_before = set.default_mode();

        assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Bypass));
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Bypass);
        assert_eq!(provider.modes(), vec![PermissionMode::Bypass]);
        assert!(
            provider.resolutions().is_empty(),
            "nothing was pending, so nothing is answered"
        );
        assert_eq!(
            set.default_mode(),
            default_before,
            "W2: a live switch is that session's only"
        );

        assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Auto));
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto);
        assert_eq!(provider.modes(), vec![PermissionMode::Bypass, PermissionMode::Auto]);
        assert_eq!(set.default_mode(), default_before);
        shut_down_all(&mut set);
    }

    #[test]
    fn a_started_tab_without_the_capability_stays_fixed() {
        let dir = workspace("tabs-switch-fixed");
        let mut set = set();

        // An empty tab is `cycle_mode`'s, not this.
        let empty = set.active();
        assert!(set.switch_mode(empty).is_err());
        assert_eq!(set.get(empty).unwrap().mode, SessionModeChoice::Auto);

        // W5: a starting tab says so, not "fixed" (false on a switch-capable sidecar).
        let connecting = set.open();
        let _tx = starting(&mut set, connecting, None, None);
        let refused = set.switch_mode(connecting).unwrap_err();
        assert_eq!(refused, "session is starting — try again once it is up");

        let fixed = set.open();
        let (provider, backend) = live(&dir);
        set.get_mut(fixed).unwrap().backend = TabBackend::Live(backend);
        let refused = set.switch_mode(fixed).unwrap_err();
        assert!(refused.contains("fixed"), "{refused}");
        assert!(provider.modes().is_empty(), "no call without the capability");
        assert_eq!(set.get(fixed).unwrap().mode, SessionModeChoice::Auto);

        // Ended, on a switchable tab: the capability check comes first.
        let ended = set.open();
        let (provider, backend) = live_switchable(&dir);
        set.get_mut(ended).unwrap().backend = TabBackend::Live(backend);
        provider.queue(AgentDomainEvent::SessionClosed { reason: "x".into() });
        until("the session's end", || {
            set.pump(&dir, true);
            set.get(ended).unwrap().wire_state() == TabStateWire::Ended
        });
        let refused = set.switch_mode(ended).unwrap_err();
        assert!(refused.contains("ended"), "{refused}");
        assert!(provider.modes().is_empty());
        shut_down_all(&mut set);
    }

    /// Review Focus 1: never a mode nobody chose.
    #[test]
    fn a_refused_switch_changes_nothing() {
        let dir = workspace("tabs-switch-refused");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live_switchable(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        provider.refuse_switches(true);

        assert!(set.switch_mode(tab).is_err());
        assert_eq!(provider.modes(), vec![PermissionMode::Bypass], "the call was made");
        assert_eq!(set.get(tab).unwrap().mode, SessionModeChoice::Auto);
        assert_eq!(
            set.get(tab).unwrap().live().unwrap().permission_mode(),
            Some(PermissionMode::Auto)
        );
        shut_down_all(&mut set);
    }

    /// W4: entering bypass answers the cards waiting on THAT tab, and no other's.
    #[test]
    fn entering_bypass_answers_this_tabs_waiting_cards_only() {
        let dir = workspace("tabs-switch-answers-own-cards");
        let mut set = set();
        let first = set.active();
        let (first_provider, backend) = live_switchable(&dir);
        set.get_mut(first).unwrap().backend = TabBackend::Live(backend);
        let second = set.open();
        let (second_provider, backend) = live_switchable(&dir);
        set.get_mut(second).unwrap().backend = TabBackend::Live(backend);

        first_provider.queue(write("p1"));
        second_provider.queue(write("p2"));
        until("both cards", || {
            set.pump(&dir, true);
            set.get(first).unwrap().attention.attention().pending == 1
                && set.get(second).unwrap().attention.attention().pending == 1
        });
        assert!(first_provider.resolutions().is_empty(), "a Write is a card in auto");

        assert_eq!(set.switch_mode(first), Ok(SessionModeChoice::Bypass));
        assert_eq!(first_provider.resolutions(), vec![("p1".to_string(), true)]);
        assert!(
            second_provider.resolutions().is_empty(),
            "another tab's card is untouched"
        );
        shut_down_all(&mut set);
    }

    /// W4: raised before the switch, delivered after -- allowed, and no card is drawn.
    #[test]
    fn a_card_delivered_after_the_switch_is_allowed() {
        let dir = workspace("tabs-switch-card-after");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live_switchable(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Bypass));

        provider.queue(write("p3-after"));
        until("the late card's answer", || {
            if let Some(payload) = set.pump(&dir, true).active_payload {
                let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
                for event in parsed["events"].as_array().unwrap() {
                    assert!(
                        !event.to_string().contains("p3-after"),
                        "no card is delivered in bypass: {event}"
                    );
                }
            }
            provider.resolutions().contains(&("p3-after".to_string(), true))
        });
        assert_eq!(provider.resolutions(), vec![("p3-after".to_string(), true)]);
        shut_down_all(&mut set);
    }

    /// W4: no card is answered on a switch back to auto; the classifier decides again.
    #[test]
    fn back_in_auto_the_classifier_decides_again() {
        let dir = workspace("tabs-switch-back-to-auto");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live_switchable(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Bypass));
        assert_eq!(set.switch_mode(tab), Ok(SessionModeChoice::Auto));

        provider.queue(write("p4"));
        pump_until_pending(&mut set, &dir, tab, 1);
        assert!(provider.resolutions().is_empty(), "a Write is a card again");

        provider.queue(read("p5"));
        until("the Read's answer", || {
            set.pump(&dir, true);
            provider.resolutions().contains(&("p5".to_string(), true))
        });
        assert_eq!(provider.resolutions(), vec![("p5".to_string(), true)]);
        assert_eq!(set.get(tab).unwrap().attention.attention().pending, 1, "p4 still waits");
        shut_down_all(&mut set);
    }

    /// The sidecar's own report moves the band, but never grants auto-approval: only an
    /// acknowledged switch does (Review Focus 2).
    #[test]
    fn a_reported_mode_change_moves_the_tab() {
        let dir = workspace("tabs-switch-reported");
        let mut set = set();
        let tab = set.active();
        let (provider, backend) = live_switchable(&dir);
        set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
        let default_before = set.default_mode();

        provider.queue(AgentDomainEvent::PermissionModeChanged {
            mode: PermissionMode::Bypass,
            provider_mode: "bypassPermissions".into(),
            floor_applied: false,
        });
        until("the reported mode", || {
            set.pump(&dir, true);
            set.get(tab).unwrap().mode == SessionModeChoice::Bypass
        });
        assert_eq!(set.default_mode(), default_before);
        assert_eq!(
            set.get(tab).unwrap().live().unwrap().permission_mode(),
            Some(PermissionMode::Auto)
        );
        shut_down_all(&mut set);
    }
}
