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

use agent::UiDelivery;

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
        }
    }
}

pub struct PumpOutput {
    /// The active tab's `events` or `snapshot` envelope, if it had any.
    pub active_payload: Option<String>,
    /// The active tab's turn trace saw its first text in this batch.
    pub first_text: bool,
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

    /// `r` (ruling 12): an ended or failed tab back to empty, keeping its number, name and mode.
    pub fn reset(&mut self, id: TabId) -> Result<Option<AgentBackend>, String> {
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        match tab.wire_state() {
            TabStateWire::Ended | TabStateWire::Failed => {}
            _ => return Err("only an ended or failed tab starts over".to_string()),
        }
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
        };
        for tab in &mut self.tabs {
            let is_active = tab.id == active;
            let on_screen = panel_mapped && is_active;
            let TabBackend::Live(backend) = &mut tab.backend else {
                // No backend holds no card: the backstop `agent_panel` had for `session.is_none()`.
                tab.attention.retain_pending(|_| false);
                continue;
            };
            let from_revision = backend.projection().last_revision;
            match backend.take_ui_delivery(project_root) {
                UiDelivery::Nothing => {}
                UiDelivery::Events(events) => {
                    tab.attention.observe(&events, on_screen);
                    if is_active {
                        if let Some(trace) = tab.turn_trace.as_mut() {
                            out.first_text = trace.observe(&events);
                        }
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
                    let pending: Vec<String> = backend.projection().pending_permissions.keys().cloned().collect();
                    tab.attention.resync(pending);
                    if is_active {
                        out.active_payload = Some(serialize_snapshot_for_js(tab.id, &SnapshotView::of(backend)));
                    } else {
                        tab.stale = true;
                    }
                }
            }
            let still: std::collections::HashSet<String> =
                backend.projection().pending_permissions.keys().cloned().collect();
            tab.attention.retain_pending(|id| still.contains(id));
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
        match &tab.backend {
            TabBackend::Live(backend) => vec![serialize_snapshot_for_js(tab.id, &SnapshotView::of(backend))],
            TabBackend::NotStarted => tab
                .last_handoff
                .iter()
                .map(|command| serialize_handoff_for_js(tab.id, command))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub fn tabs_payload(&self) -> String {
        let resumable = self.kind == BackendKind::Sidecar;
        let views: Vec<TabView> = self.tabs.iter().map(|t| t.view(resumable)).collect();
        serialize_tabs_for_js(self.active, &views)
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

    pub fn close_facts(&self, id: TabId) -> Option<tabs::CloseFacts> {
        let tab = self.get(id)?;
        Some(tabs::CloseFacts {
            number: tab.number,
            label_name: tab.label_name(),
            turn_running: tab.turn_running(),
            queued: 0,
            legacy: self.kind == BackendKind::Legacy,
            has_backend: tab.live().is_some(),
        })
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
}
