//! A chat tab moving to the other window of a linked pair, the panel's half: the tab frozen with its page's view of
//! it and its agent's provider, the record that crosses, the tab let go or taken back, and in the other window the
//! tab taken up from that record and its arriving turn held by this window's presence first.
//!
//! **The freeze answers later.** The window asks for a freeze and goes on; the panel first asks the page showing the
//! tab for its view of it (one message each way, waited for at most [`VIEW_WAIT`]: a page that does not answer moves
//! the tab without it), then freezes the tab in the tab set, then its agent's provider, and posts the record and the
//! descriptors, or why it could not, through the window's hook. A freeze the conversation is not ready for (a batch
//! being folded, a running turn's session not bound yet) is asked again on the next tick for a bounded while.
//!
//! **What crosses** is one JSON record and the descriptors: the tab set's record of the tab ([`TabHandoff`]), its
//! conversation's ([`FrozenChat`]), the provider's own record and kind, whether the session lease's descriptor
//! follows the provider's, the panel's per-tab record ([`PanelTabHandoff`]) and the page's view, which only the page
//! reads. In the other window all of it is the other window's input: read with the typed records, its shape and its
//! descriptor count checked before anything is built, and refused whole otherwise.
//!
//! **No editor call crosses.** Before the source's provider freezes, each call of the tab is told to try again and
//! each of its cards goes ([`editor_glue::tab_leaving`]). After the commit the destination
//! holds the session's calls until the session has said which are open ([`editor_glue::tab_arrived`]).
//!
//! **A bridge socket crosses by name.** The agent's tool server stays where it is and connects again by the path it
//! was started with. Once the provider froze the source lets the name go, because a refused freeze keeps the session
//! here and a session here keeps its socket; the destination binds it when the tab is taken up, and a move called
//! off binds it here again ([`editor_glue::tab_brought`], [`editor_glue::bridge_back`]).
//!
//! **Nothing is written to the agent before the commit.** The arriving tab is taken up only when the window carries
//! out the commit's adopt, and its provider then starts reading and answering at once. Before that the window holds
//! the arriving session in its presence ([`hold_presence`]), under the id the tab then takes.

use super::*;
#[cfg(target_os = "linux")]
use agent::chat_mover::{adopt, Adopted, ChatMover, FrozenProvider, MovedKind};
use eitri_core::agent_bridge::{serialize_view_export_for_js, serialize_view_import_for_js};
use eitri_core::tab_set::{FrozenChat, FrozenTab, MoveRefusedWhy, TabHandoff};
use eitri_core::turn_review::Held;
use eitri_core::wm::ChatMoveFacts;
use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};
#[cfg(not(target_os = "linux"))]
use unmovable::{adopt, Adopted, ChatMover, FrozenProvider, MovedKind};

/// How long the page is given to answer with its view of the tab before the tab moves without it.
pub(super) const VIEW_WAIT: Duration = Duration::from_secs(1);

/// How long a freeze is asked again while the conversation folds a batch or the running turn's session is not
/// bound to its review yet: the window's own deadline for the freeze is longer.
const RETRY_FOR: Duration = Duration::from_secs(3);

/// The largest view of a tab a page may hand over: a cursor, a mode, a scroll position and the blocks expanded.
pub(super) const VIEW_MAX_BYTES: usize = 64 * 1024;

/// The record's own version: a record of another shape is refused whole.
const RECORD_VERSION: u64 = 1;

/// The way `backend`'s provider moves with its chat, if it has one.
#[cfg(target_os = "linux")]
fn mover_of(backend: &AgentBackend) -> Option<&dyn ChatMover> {
    backend.mover()
}

/// Where the providers cannot hand their processes and descriptors to another process, no provider has a way to
/// move with its chat.
#[cfg(not(target_os = "linux"))]
fn mover_of(_backend: &AgentBackend) -> Option<&dyn ChatMover> {
    None
}

/// The shapes the panel's move code is written against, where no provider can move with its chat: a chat with a
/// session never moves from here (no provider offers a mover), and an arriving one with a session is refused whole,
/// since its record names no agent this build can take up. A tab with no session still moves.
#[cfg(not(target_os = "linux"))]
pub(super) mod unmovable {
    use std::io;
    use std::os::fd::OwnedFd;
    use std::sync::Arc;

    /// No agent's provider moves here, so no kind can be named.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum MovedKind {}

    impl MovedKind {
        pub(crate) fn wire(self) -> &'static str {
            match self {}
        }
        pub(crate) fn from_wire(_name: &str) -> Option<MovedKind> {
            None
        }
        pub(crate) fn descriptors(self) -> usize {
            match self {}
        }
    }

    pub(crate) struct FrozenProvider {
        pub kind: MovedKind,
        pub record: serde_json::Value,
        pub fds: Vec<OwnedFd>,
    }

    impl FrozenProvider {
        pub(crate) fn copies(&self) -> io::Result<Vec<OwnedFd>> {
            self.fds.iter().map(OwnedFd::try_clone).collect()
        }
    }

    pub(crate) trait ChatMover {
        fn live_move(&self) -> bool;
        fn freeze_for_chat_move(&self) -> Result<FrozenProvider, String>;
        fn resume_after_chat_move(&self, frozen: FrozenProvider) -> Result<(), String>;
        fn commit_chat_move(&self) -> io::Result<()>;
        fn forget_chat_move(&self);
    }

    pub(crate) struct Adopted {
        pub agent: Arc<dyn agent::AgentProvider + Send + Sync>,
        pub external: Option<Arc<dyn agent::ExternalProvider + Send + Sync>>,
    }

    pub(crate) fn adopt(kind: MovedKind, _record: &serde_json::Value, _fds: Vec<OwnedFd>) -> Result<Adopted, String> {
        match kind {}
    }
}

/// What the panel tells the window about a move, which the window turns into the controller's chat events.
#[derive(Debug)]
pub enum ChatMoveEvent {
    /// `view`'s tab froze for move `mv`: the record and the descriptors that go with it.
    Frozen {
        view: ViewId,
        mv: u64,
        record: serde_json::Value,
        fds: Vec<OwnedFd>,
    },
    /// It could not freeze for move `mv`; the tab is as it was.
    FreezeRefused { view: ViewId, mv: u64, why: String },
    /// The presence holds the session arriving with move `mv`.
    PresenceHeld { mv: u64 },
    /// The presence could not hold it.
    PresenceRefused { mv: u64, why: String },
    /// A tab handed to or from another window by resume, which no move of the controller's carries.
    HandOver(super::TabAnswer),
    /// The base view's last tab was closed by asking, and a fresh one stands in its place.
    LastTabClosed,
}

/// What takes a moved chat's provider up by its kind: the providers' own adopt, or a test's stand-in.
pub(super) type Adopter = fn(MovedKind, &serde_json::Value, Vec<OwnedFd>) -> Result<Adopted, String>;

/// The panel's moves in flight.
pub(super) struct ChatMoves {
    /// Arriving empty tabs are real tabs, so a later arrival must not replace them as a fresh placeholder.
    adopted_tabs: BTreeSet<TabId>,
    /// Freezes asked for and not answered yet, by move.
    freezing: BTreeMap<u64, Freezing>,
    /// Tabs frozen and sent, with what their provider gave: what a move called off takes back.
    frozen: BTreeMap<TabId, Frozen>,
    /// Arrivals whose presence answer is awaited, by move.
    arrivals: BTreeMap<u64, Arrival>,
    /// The number of the next `view_export` asked of a page.
    next_request: u64,
    pub(super) adopter: Adopter,
    /// Where what the panel tells the window goes; `None` in a window with no other window, which asks nothing.
    pub(super) hook: Option<Rc<dyn Fn(ChatMoveEvent)>>,
    /// The other page's views of tabs that arrived while the page of their view had no document ready yet (a
    /// numbered view's page is made when its move is accepted and may still be loading at the commit): each is
    /// given to that page at its `ready`, ahead of the tabs that show it, since a dispatch into a page with no
    /// document is lost.
    imports: Vec<(ViewId, TabId, serde_json::Value)>,
}

impl ChatMoves {
    /// Whether a freeze waits for its page, or an arrival for its presence: the tick then runs on its steady
    /// cadence, since neither raises the wake.
    pub(super) fn is_waiting(&self) -> bool {
        !self.freezing.is_empty() || !self.arrivals.is_empty()
    }
}

impl Default for ChatMoves {
    fn default() -> Self {
        ChatMoves {
            adopted_tabs: BTreeSet::new(),
            freezing: BTreeMap::new(),
            frozen: BTreeMap::new(),
            arrivals: BTreeMap::new(),
            next_request: 1,
            adopter: adopt,
            hook: None,
            imports: Vec::new(),
        }
    }
}

struct Freezing {
    view: ViewId,
    tab: TabId,
    request: u64,
    asked: Instant,
    /// The page's answer: `None` until it comes, then its view (`None` inside for a page that holds none).
    answer: Option<Option<serde_json::Value>>,
    /// When the conversation first said to ask again.
    retry_since: Option<Instant>,
}

struct Frozen {
    view: ViewId,
    /// What the provider gave at the freeze; the window sent copies of its descriptors.
    provider: Option<FrozenProvider>,
}

struct Arrival {
    answer: mpsc::Receiver<Held>,
}

/// The tab of `view` a move would take: an extra view's one tab; in the base view the tab holding `session` when
/// one is named, else the active tab.
fn moving_tab(state: &AgentPanelState, view: ViewId, session: Option<&str>) -> Result<TabId, String> {
    if !view.is_base() {
        return state
            .tabs
            .tab_in(view)
            .ok_or_else(|| "this chat view holds no tab".to_string());
    }
    let Some(session) = session else {
        return Ok(state.tabs.active());
    };
    state
        .tabs
        .base_tabs()
        .into_iter()
        .find(|tab| {
            state
                .tabs
                .get(*tab)
                .and_then(Tab::provider_session_id)
                .is_some_and(|held| held == session)
        })
        .ok_or_else(|| "no tab of this view holds that session".to_string())
}

/// What a move of `view`'s tab would take, or why it cannot move now, read without changing anything.
pub(super) fn chat_move_facts(
    state: &AgentPanelState,
    view: ViewId,
    session: Option<&str>,
) -> Result<ChatMoveFacts, String> {
    if state.shutting_down {
        return Err("the window is closing".into());
    }
    let tab = moving_tab(state, view, session)?;
    facts_for_tab(state, tab)
}

/// A particular tab's facts, including an unstarted tab that has no provider session id to name it by.
pub(super) fn facts_for_tab(state: &AgentPanelState, tab: TabId) -> Result<ChatMoveFacts, String> {
    if state.shutting_down {
        return Err("the window is closing".into());
    }
    if state.chat_moves.frozen.contains_key(&tab) || state.chat_moves.freezing.values().any(|f| f.tab == tab) {
        return Err(MoveRefusedWhy::AlreadyMoving.to_string());
    }
    if let Some(why) = state.tabs.move_blocker(tab) {
        return Err(why.to_string());
    }
    panel_tab_handoff(state, tab)?;
    let t = state.tabs.get(tab).ok_or_else(|| MoveRefusedWhy::NoTab.to_string())?;
    let live = match t.live() {
        None => true,
        Some(backend) => mover_of(backend)
            .ok_or_else(|| "this chat's agent cannot move to another window".to_string())?
            .live_move(),
    };
    Ok(ChatMoveFacts {
        tab,
        session: t.provider_session_id(),
        live,
        running: t.turn_running(),
    })
}

/// The view whose tabs hold `session`, if any tab here does.
pub(super) fn view_of_session(state: &AgentPanelState, session: &str) -> Option<ViewId> {
    let tab = state
        .tabs
        .tabs()
        .iter()
        .find(|t| t.provider_session_id().as_deref() == Some(session))?;
    Some(state.tabs.view_of(tab.id))
}

/// The window asks for `view`'s tab (or `tab`) to freeze for move `mv`: the page showing it is asked for its view of
/// it first, and the freeze follows on the page's answer, or at once with no page to ask.
pub(super) fn begin_freeze(state: &mut AgentPanelState, view: ViewId, tab: Option<TabId>, mv: u64, out: &mut Outbox) {
    let tab = match tab.map_or_else(|| moving_tab(state, view, None), Ok) {
        Ok(tab) => tab,
        Err(why) => {
            out.effect(Effect::ChatMove(ChatMoveEvent::FreezeRefused { view, mv, why }));
            return;
        }
    };
    let request = state.chat_moves.next_request;
    state.chat_moves.next_request += 1;
    let shown = state.shown_in(tab).filter(|page| state.ready(*page));
    let answer = match shown {
        Some(page) => {
            out.send(page, serialize_view_export_for_js(request, tab));
            None
        }
        // No page shows it: there is no view to wait for.
        None => Some(None),
    };
    state.chat_moves.freezing.insert(
        mv,
        Freezing {
            view,
            tab,
            request,
            asked: Instant::now(),
            answer,
            retry_since: None,
        },
    );
    poll_chat_moves(state, out);
}

/// The page `page` answered `view_export` number `request` with its view of the tab: the freeze waiting for it goes
/// on. A view too large, or not an object, is dropped and the tab moves without it.
pub(super) fn view_exported(
    state: &mut AgentPanelState,
    page: ViewId,
    request: u64,
    view: Option<serde_json::Value>,
    out: &mut Outbox,
) {
    let Some(freezing) = state
        .chat_moves
        .freezing
        .values_mut()
        .find(|f| f.request == request && f.answer.is_none())
    else {
        return;
    };
    let view = view.filter(|v| {
        let fits = v.is_object() && v.to_string().len() <= VIEW_MAX_BYTES;
        if !fits {
            eprintln!("[chat_move] {page:?}: the page's view of the tab is not one to carry; it moves without it");
        }
        fits
    });
    freezing.answer = Some(view);
    poll_chat_moves(state, out);
}

/// What one freeze attempt came to.
enum Attempt {
    Done {
        record: serde_json::Value,
        fds: Vec<OwnedFd>,
        provider: Option<FrozenProvider>,
    },
    Again,
    Refused(String),
}

/// Each tick and after each answer: a freeze whose page answered (or did not in time) is made, and a presence that
/// answered is told to the window.
pub(super) fn poll_chat_moves(state: &mut AgentPanelState, out: &mut Outbox) {
    let now = Instant::now();
    let due: Vec<u64> = state
        .chat_moves
        .freezing
        .iter()
        .filter(|(_, f)| f.answer.is_some() || now.saturating_duration_since(f.asked) >= VIEW_WAIT)
        .map(|(mv, _)| *mv)
        .collect();
    for mv in due {
        let Some(mut freezing) = state.chat_moves.freezing.remove(&mv) else {
            continue;
        };
        let (view, tab) = (freezing.view, freezing.tab);
        let blob = freezing.answer.clone().flatten();
        match freeze(state, tab, blob, out) {
            Attempt::Done { record, fds, provider } => {
                state.chat_moves.frozen.insert(tab, Frozen { view, provider });
                println!("[chat_move] tab {}: frozen for move {mv}", tab.0);
                out.effect(Effect::ChatMove(ChatMoveEvent::Frozen { view, mv, record, fds }));
            }
            Attempt::Again => {
                let since = *freezing.retry_since.get_or_insert(now);
                if now.saturating_duration_since(since) < RETRY_FOR {
                    if freezing.answer.is_none() {
                        freezing.answer = Some(None);
                    }
                    state.chat_moves.freezing.insert(mv, freezing);
                } else {
                    let why = MoveRefusedWhy::Folding.to_string();
                    out.effect(Effect::ChatMove(ChatMoveEvent::FreezeRefused { view, mv, why }));
                }
            }
            Attempt::Refused(why) => {
                println!("[chat_move] tab {}: not frozen for move {mv}: {why}", tab.0);
                out.effect(Effect::ChatMove(ChatMoveEvent::FreezeRefused { view, mv, why }));
            }
        }
    }
    let answered: Vec<(u64, Result<(), String>)> = state
        .chat_moves
        .arrivals
        .iter()
        .filter_map(|(mv, arrival)| match arrival.answer.try_recv() {
            Ok(Held::Yes) => Some((*mv, Ok(()))),
            Ok(Held::No(why)) => Some((*mv, Err(why))),
            Ok(Held::NotYet) | Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some((*mv, Err("the presence thread has ended".to_string()))),
        })
        .collect();
    for (mv, held) in answered {
        if state.chat_moves.arrivals.remove(&mv).is_none() {
            continue;
        }
        match held {
            Ok(()) => out.effect(Effect::ChatMove(ChatMoveEvent::PresenceHeld { mv })),
            Err(why) => {
                state.tabs.drop_arrival(mv);
                out.effect(Effect::ChatMove(ChatMoveEvent::PresenceRefused { mv, why }));
            }
        }
    }
}

/// Freezes `tab` and its provider, and builds the record that crosses. Anything refused after the tab froze unfreezes
/// it (and the provider, if it froze): a refused freeze leaves the tab as it was.
fn freeze(state: &mut AgentPanelState, tab: TabId, view: Option<serde_json::Value>, out: &mut Outbox) -> Attempt {
    if state.shutting_down {
        return Attempt::Refused("the window is closing".into());
    }
    let panel = match panel_tab_handoff(state, tab) {
        Ok(panel) => panel,
        Err(why) => return Attempt::Refused(why),
    };
    let FrozenTab { tab: record, chat } = match state.tabs.freeze_for_move(tab) {
        Ok(frozen) => frozen,
        Err(why) if why.is_retryable() => return Attempt::Again,
        Err(why) => return Attempt::Refused(why.to_string()),
    };
    // Here and no later: a frozen provider takes no answer. A freeze refused below leaves the agent told to try again.
    editor_glue::tab_leaving(state, tab, out);
    let taken = match &chat {
        None => Ok((None, None)),
        Some(_) => freeze_provider(state, tab),
    };
    let (provider, lease) = match taken {
        Ok(taken) => taken,
        Err(why) => {
            state.tabs.unfreeze(tab);
            return Attempt::Refused(why);
        }
    };
    editor_glue::bridge_leaving(state, tab);
    let built =
        build_record(&record, chat.as_ref(), provider.as_ref(), lease.is_some(), &panel, view).and_then(|record| {
            let mut fds = match &provider {
                Some(provider) => provider.copies().map_err(|e| format!("the agent's descriptors: {e}"))?,
                None => Vec::new(),
            };
            fds.extend(lease);
            Ok((record, fds))
        });
    match built {
        Ok((record, fds)) => Attempt::Done { record, fds, provider },
        Err(why) => {
            take_back(state, tab, provider);
            Attempt::Refused(why)
        }
    }
}

/// The frozen tab's provider frozen too, with the session lease's descriptor when the session holds one.
fn freeze_provider(state: &AgentPanelState, tab: TabId) -> Result<(Option<FrozenProvider>, Option<OwnedFd>), String> {
    let backend = state
        .tabs
        .get(tab)
        .and_then(Tab::live)
        .ok_or_else(|| "the tab's session ended".to_string())?;
    let mover = mover_of(backend).ok_or_else(|| "this chat's agent cannot move to another window".to_string())?;
    let provider = mover.freeze_for_chat_move()?;
    let lease = match backend.lease_descriptor() {
        None => None,
        Some(Ok(fd)) => Some(fd),
        Some(Err(err)) => {
            if let Err(why) = mover.resume_after_chat_move(provider) {
                eprintln!("[chat_move] tab {}: its agent could not be taken back: {why}", tab.0);
            }
            return Err(format!("the session's lease could not be handed over: {err}"));
        }
    };
    Ok((Some(provider), lease))
}

fn build_record(
    tab: &TabHandoff,
    chat: Option<&FrozenChat>,
    provider: Option<&FrozenProvider>,
    lease: bool,
    panel: &PanelTabHandoff,
    view: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let written = |what: &str, err: serde_json::Error| format!("the tab's {what} could not be written: {err}");
    let tab = serde_json::to_value(tab).map_err(|e| written("record", e))?;
    let chat = match chat {
        Some(chat) => serde_json::to_value(chat).map_err(|e| written("conversation", e))?,
        None => serde_json::Value::Null,
    };
    let provider = match provider {
        Some(p) => serde_json::json!({ "kind": p.kind.wire(), "record": p.record }),
        None => serde_json::Value::Null,
    };
    Ok(serde_json::json!({
        "version": RECORD_VERSION,
        "tab": tab,
        "chat": chat,
        "provider": provider,
        "lease": lease,
        "panel": panel.to_value(),
        "view": view.unwrap_or(serde_json::Value::Null),
    }))
}

/// A frozen tab taken back: its provider reads on and the tab takes input again.
fn take_back(state: &mut AgentPanelState, tab: TabId, provider: Option<FrozenProvider>) {
    if let Some(provider) = provider {
        let mover = state.tabs.get(tab).and_then(Tab::live).and_then(mover_of);
        match mover {
            Some(mover) => {
                if let Err(why) = mover.resume_after_chat_move(provider) {
                    eprintln!("[chat_move] tab {}: its agent could not be taken back: {why}", tab.0);
                }
            }
            None => eprintln!("[chat_move] BUG: tab {} froze an agent it no longer holds", tab.0),
        }
    }
    state.tabs.unfreeze(tab);
    editor_glue::bridge_back(state, tab);
}

/// The frozen tab of `view` the window names (`tab`), or the one `view` has frozen when it names none.
fn frozen_tab(state: &AgentPanelState, view: ViewId, tab: Option<TabId>) -> Option<TabId> {
    match tab {
        Some(tab) => state.chat_moves.frozen.contains_key(&tab).then_some(tab),
        None => state
            .chat_moves
            .frozen
            .iter()
            .find(|(_, frozen)| frozen.view == view)
            .map(|(tab, _)| *tab),
    }
}

/// The move did not happen: a freeze still waiting for its page is forgotten, and a frozen tab is taken back.
pub(super) fn resume_moved(state: &mut AgentPanelState, view: ViewId, tab: Option<TabId>) {
    state
        .chat_moves
        .freezing
        .retain(|_, f| !(f.view == view && tab.is_none_or(|tab| f.tab == tab)));
    let Some(tab) = frozen_tab(state, view, tab) else {
        return;
    };
    let frozen = state.chat_moves.frozen.remove(&tab);
    take_back(state, tab, frozen.and_then(|f| f.provider));
    println!("[chat_move] tab {}: taken back; it reads and answers again", tab.0);
}

/// The other window holds the tab now: its provider acts for the session no more, and the tab leaves this window
/// without its session being ended. Nothing the move passed on is closed: the other window holds its own copies.
pub(super) fn forget_moved(
    state: &mut AgentPanelState,
    view: ViewId,
    tab: Option<TabId>,
    out: &mut Outbox,
) -> Result<(), String> {
    let tab = frozen_tab(state, view, tab).ok_or_else(|| "no tab of this view is frozen for a move".to_string())?;
    let frozen = state.chat_moves.frozen.remove(&tab);
    if let Some(mover) = state.tabs.get(tab).and_then(Tab::live).and_then(mover_of) {
        mover.forget_chat_move();
    }
    drop(frozen);
    state.chat_moves.adopted_tabs.remove(&tab);
    let shown_in = state.tabs.view_of(tab);
    // The session is the other window's: its backend goes without a shutdown.
    let left = state.tabs.hand_over(tab);
    drop(left);
    state.trust.forget_tab(tab);
    editor_glue::tab_gone(state, tab);
    state.shown_hints.retain(|hint| hint.tab != tab.0);
    state.review_hints.retain(|hint| hint.tab != tab.0);
    if shown_in.is_base() {
        switch_payloads(state, out);
    } else {
        let payload = tabs_payload_recorded(state, shown_in);
        out.send(shown_in, payload);
        state.emptied_views.push(shown_in);
    }
    send_hello_if_open_sessions_changed(state, out);
    println!("[chat_move] tab {}: in the other window now", tab.0);
    Ok(())
}

/// The window will commit a move bringing `session`: the presence counts its running turn from now on, and the
/// window is told once it holds it.
pub(super) fn hold_presence(state: &mut AgentPanelState, session: &str, mv: u64, out: &mut Outbox) {
    match state.tabs.hold_arrival(session, mv) {
        // No presence here to hold: nothing to wait for.
        None => out.effect(Effect::ChatMove(ChatMoveEvent::PresenceHeld { mv })),
        Some(answer) => {
            state.chat_moves.arrivals.insert(mv, Arrival { answer });
            poll_chat_moves(state, out);
        }
    }
}

/// Move `mv` was adopted or given up: the presence lets only its arrival hold go.
pub(super) fn drop_presence(state: &mut AgentPanelState, _session: &str, mv: u64) {
    state.chat_moves.arrivals.remove(&mv);
    state.tabs.drop_arrival(mv);
}

/// What a moved chat's record says, read as the other window's input.
struct Arriving {
    tab: TabHandoff,
    chat: Option<(FrozenChat, MovedKind, serde_json::Value)>,
    lease: bool,
    panel: PanelTabHandoff,
    view: Option<serde_json::Value>,
}

fn read_record(record: &serde_json::Value, fds: usize) -> Result<Arriving, String> {
    let object = record
        .as_object()
        .ok_or_else(|| "the moved chat's record is not an object".to_string())?;
    let field = |name: &str| object.get(name).unwrap_or(&serde_json::Value::Null);
    if field("version").as_u64() != Some(RECORD_VERSION) {
        return Err(format!("the moved chat's record is not version {RECORD_VERSION}"));
    }
    let unreadable = |what: &str, err: serde_json::Error| format!("the moved chat's {what} is not readable: {err}");
    let tab: TabHandoff = serde_json::from_value(field("tab").clone()).map_err(|e| unreadable("tab", e))?;
    let chat = match (field("chat"), field("provider")) {
        (serde_json::Value::Null, serde_json::Value::Null) => None,
        (chat, provider) => {
            let chat: FrozenChat = serde_json::from_value(chat.clone()).map_err(|e| unreadable("conversation", e))?;
            let kind = provider
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .and_then(MovedKind::from_wire)
                .ok_or_else(|| "the moved chat names no agent this build moves".to_string())?;
            let record = provider
                .get("record")
                .cloned()
                .ok_or_else(|| "the moved chat's agent has no record".to_string())?;
            Some((chat, kind, record))
        }
    };
    if chat.is_some() != tab.has_session() {
        return Err("the moved chat's record and its session do not agree".into());
    }
    let lease = field("lease")
        .as_bool()
        .ok_or_else(|| "the moved chat's record does not say whether its lease came".to_string())?;
    if lease && chat.is_none() {
        return Err("a moved tab with no session brought a lease".into());
    }
    let panel = PanelTabHandoff::from_value(field("panel"))?;
    let view = Some(field("view").clone()).filter(|v| v.is_object() && v.to_string().len() <= VIEW_MAX_BYTES);
    let expected = chat.as_ref().map_or(0, |(_, kind, _)| kind.descriptors()) + usize::from(lease);
    if fds != expected {
        return Err(format!("the moved chat brought {fds} descriptors, not {expected}"));
    }
    Ok(Arriving {
        tab,
        chat,
        lease,
        panel,
        view,
    })
}

/// The tab of the base view nobody has touched yet (its first and only tab, empty), which an arrival takes the place
/// of.
fn untouched_only_tab(state: &AgentPanelState) -> Option<TabId> {
    let base = state.tabs.base_tabs();
    let [only] = base.as_slice() else {
        return None;
    };
    let t = state.tabs.get(*only)?;
    let untouched = !state.chat_moves.adopted_tabs.contains(only)
        && matches!(t.backend(), TabBackend::NotStarted)
        && t.name.is_none()
        && t.draft().is_empty()
        && t.session.queue.is_empty()
        && !state.tabs.is_moving(*only);
    untouched.then_some(*only)
}

/// The views held for `view`'s page until its document was ready, as the envelopes that give them to it; those of
/// tabs that have left this window since go without being given.
pub(super) fn take_held_imports(state: &mut AgentPanelState, view: ViewId) -> Vec<String> {
    let held = std::mem::take(&mut state.chat_moves.imports);
    let mut payloads = Vec::new();
    for (page, tab, blob) in held {
        if page != view {
            state.chat_moves.imports.push((page, tab, blob));
        } else if state.tabs.get(tab).is_some() {
            payloads.push(serialize_view_import_for_js(tab, &blob));
        }
    }
    payloads
}

/// The commit's adopt: the tab another window moved here is taken up in `view` from `record` and `fds`, its
/// provider reading and answering from now on and its lease naming this window. The page is given the other page's
/// view of the tab before it is shown it. `Err` with nothing taken up when the record is not one to take up.
pub(super) fn adopt_moved(
    state: &mut AgentPanelState,
    view: ViewId,
    record: &serde_json::Value,
    fds: Vec<OwnedFd>,
    out: &mut Outbox,
) -> Result<TabId, String> {
    let Arriving {
        tab,
        chat,
        lease,
        panel,
        view: blob,
    } = read_record(record, fds.len())?;
    let replaced = if view.is_base() {
        untouched_only_tab(state)
    } else {
        None
    };
    let mut fds = fds.into_iter();
    let id = match chat {
        None => state.tabs.adopt_moved_unstarted(tab, view)?,
        Some((chat, kind, provider_record)) => {
            let provider_fds: Vec<OwnedFd> = fds.by_ref().take(kind.descriptors()).collect();
            let lease_fd = if lease { fds.next() } else { None };
            let session = chat.conversation.provider_session_id.clone();
            let folded_through = chat.core.folded_through;
            let provider_name = chat.conversation.provider_name.clone();
            let cwd = chat.conversation.canonical_cwd.to_string_lossy().into_owned();
            let Adopted { agent, external } = (state.chat_moves.adopter)(kind, &provider_record, provider_fds)?;
            let lease = match (lease_fd, session.as_deref()) {
                (Some(fd), Some(session)) => Some(
                    agent::lease::SessionLease::from_moved(fd, &provider_name, &cwd, session)
                        .map_err(|e| format!("the moved chat's lease is not its session's: {e}"))?,
                ),
                (Some(_), None) => return Err("a moved chat with no session brought a lease".into()),
                (None, _) => None,
            };
            let backend = AgentBackend::adopt_moved(
                chat.core,
                chat.conversation,
                chat.backend,
                agent::MovedProvider { agent, external, lease },
            )
            .map_err(|e| e.message)?;
            let id = state.tabs.adopt_moved(tab, backend, view);
            commit_adopted(state, id, folded_through);
            id
        }
    };
    adopt_panel_tab(state, id, panel);
    state.chat_moves.adopted_tabs.retain(|id| state.tabs.get(*id).is_some());
    state.chat_moves.adopted_tabs.insert(id);
    if let Some(gone) = replaced {
        if let Some(removed) = state.tabs.remove(gone) {
            let owed = state.retiring.tab(removed);
            debug_assert!(owed.is_empty(), "an untouched tab owes nothing");
        }
    }
    let page = if view.is_base() { ViewId::BASE } else { view };
    if let Some(blob) = blob {
        if state.ready(page) {
            out.send(page, serialize_view_import_for_js(id, &blob));
        } else {
            state.chat_moves.imports.push((page, id, blob));
        }
    }
    if view.is_base() {
        switch_payloads(state, out);
    } else {
        let payload = tabs_payload_recorded(state, view);
        out.send(view, payload);
    }
    send_hello_if_open_sessions_changed(state, out);
    println!("[chat_move] a tab arrived as tab {}", id.0);
    Ok(id)
}

/// The adopted tab's provider starts reading and answering, the lease names this process, and the session is asked
/// which of its editor calls are open.
fn commit_adopted(state: &mut AgentPanelState, id: TabId, folded_through: u64) {
    let Some(backend) = state.tabs.get_mut(id).and_then(Tab::live_mut) else {
        return;
    };
    if let Some(mover) = mover_of(backend) {
        if let Err(err) = mover.commit_chat_move() {
            eprintln!("[chat_move] tab {}: its agent could not be taken over: {err}", id.0);
        }
    }
    if let Err(err) = backend.commit_moved() {
        eprintln!(
            "[chat_move] tab {}: its lease still names the other window: {err}",
            id.0
        );
    }
    editor_glue::tab_arrived(state, id, folded_through);
}

// The tests drive the providers' own movers and their stand-ins, which exist only where providers move.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::agent_panel::editor_glue::tests::bridged::{serve, Peer};
    use crate::agent_panel::editor_glue::tests::state_reply;
    use crate::agent_panel::tests::*;
    use eitri_core::editor_tools::testing::FakeEditor;
    use eitri_core::test_providers::{adopt_moving, MovingProvider};
    use std::sync::Arc;

    const SESSION: &str = "sess-moving-tab";

    /// One window of the pair: its panel over a recording page that has said `ready`, and what its move hook heard.
    struct Window {
        handle: AgentPanelHandle,
        page: Rc<RecordingPage>,
        heard: Rc<RefCell<Vec<ChatMoveEvent>>>,
    }

    fn window() -> Window {
        let (handle, page, _host) = handle_with_recording_page();
        page.visible.set(true);
        handle.page_message(ViewId::BASE, READY);
        let heard = Rc::new(RefCell::new(Vec::new()));
        let into = heard.clone();
        handle.on_chat_move(move |event| into.borrow_mut().push(event));
        handle.state.borrow_mut().chat_moves.adopter = adopt_moving;
        Window { handle, page, heard }
    }

    fn workspace(label: &str) -> PathBuf {
        agent::state_dirs::redirect_state_to_a_test_root();
        agent::state_dirs::test_workspace_dir(label)
    }

    /// The window's active tab, given a live Claude conversation over a moving double, its session opened and
    /// one turn finished, the tab set pumped until it shows.
    fn moving_tab(w: &Window, dir: &Path) -> (TabId, Arc<MovingProvider>) {
        session_tab(w, dir, RecordingProvider::default(), None)
    }

    fn session_tab(
        w: &Window,
        dir: &Path,
        provider: RecordingProvider,
        tools: Option<agent::ClientToolSet>,
    ) -> (TabId, Arc<MovingProvider>) {
        let provider = MovingProvider::new(provider, true);
        let conversation =
            agent::AgentConversation::create_with_tools(provider.clone(), dir, ProjectTrust::Untrusted, tools).unwrap();
        let tab = {
            let mut state = w.handle.state.borrow_mut();
            let tab = state.tabs.active();
            state
                .tabs
                .get_mut(tab)
                .unwrap()
                .set_backend(TabBackend::Live(AgentBackend::Conversation {
                    agent: AgentId::Claude,
                    conversation: Box::new(conversation),
                }));
            tab
        };
        provider.inner.open_session(SESSION, dir);
        stream(&provider.inner, "t1", "first answer");
        provider.inner.queue(completed("t1"));
        tick_handle_until(&w.handle, "the first turn's end", || {
            w.handle
                .state
                .borrow()
                .tabs
                .get(tab)
                .and_then(Tab::live)
                .is_some_and(|b| {
                    let projection = b.projection();
                    !projection.transcript.is_empty()
                        && projection.active_turn_id.is_none()
                        && b.provider_session_id().is_some()
                })
        });
        // The conversation folds on its own thread, so all of that can show before the first tick, and a window
        // that has never pumped its tabs refuses a freeze.
        w.handle.tick();
        (tab, provider)
    }

    fn completed(turn: &str) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::TurnCompleted {
            turn_id: turn.into(),
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        }
    }

    /// The `view_export` the page was last asked, as its request number.
    fn export_asked(page: &RecordingPage, tab: TabId) -> Option<u64> {
        page.sent
            .borrow()
            .iter()
            .rev()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "view_export" && v["tab"] == tab.0)
            .find_map(|v| v["request"].as_u64())
    }

    fn page_view() -> serde_json::Value {
        serde_json::json!({
            "cursor": 3, "mode": "browse", "expanded": { "row-2": true }, "scrollTop": 120.5,
            "atBottom": false, "detailed": true, "unseenAfterSeq": null
        })
    }

    /// The page answers the freeze's question with `view`.
    fn answer_export(w: &Window, request: u64, view: serde_json::Value) {
        w.handle.page_message(
            ViewId::BASE,
            &serde_json::json!({ "type": "view_exported", "request_id": "v-1", "request": request, "view": view })
                .to_string(),
        );
    }

    /// The frozen tab's record and descriptors, as the window heard them for move `mv`.
    fn frozen(w: &Window, mv: u64) -> (serde_json::Value, Vec<OwnedFd>) {
        let froze = |e: &ChatMoveEvent| matches!(e, ChatMoveEvent::Frozen { mv: m, .. } if *m == mv);
        // This freeze and no other answer: a window that took the tab up has heard its own `PresenceHeld`, and a freeze
        // asked again while the conversation folds a batch answers on a later tick.
        tick_handle_until(&w.handle, "the freeze's answer", || w.heard.borrow().iter().any(froze));
        let mut heard = w.heard.borrow_mut();
        let at = heard.iter().position(froze).expect("it was heard above");
        match heard.remove(at) {
            ChatMoveEvent::Frozen { record, fds, .. } => (record, fds),
            _ => unreachable!(),
        }
    }

    /// Ticks until the window has heard an answer: a freeze asked again while the conversation folds a batch
    /// answers on a later tick.
    fn heard_back(w: &Window) {
        tick_handle_until(&w.handle, "the freeze's answer", || !w.heard.borrow().is_empty());
    }

    fn projection_of(w: &Window, tab: TabId) -> serde_json::Value {
        let state = w.handle.state.borrow();
        let backend = state.tabs.get(tab).and_then(Tab::live).expect("a live tab");
        let projection = serde_json::to_value(&*backend.projection()).unwrap();
        projection
    }

    /// Freezes the source's tab for move 1 with the page's view, takes it up in the destination's base view after
    /// its presence answered, and lets it go in the source: the move as both windows carry it out.
    fn move_tab(src: &Window, dst: &Window, tab: TabId) -> (TabId, serde_json::Value) {
        src.handle.freeze_for_move(ViewId::BASE, Some(tab), 1);
        let request = export_asked(&src.page, tab).expect("the page is asked for its view of the tab");
        answer_export(src, request, page_view());
        let (record, fds) = frozen(src, 1);
        land(src, dst, tab, record, fds)
    }

    /// The second half of [`move_tab`], for a test that froze the tab itself.
    fn land(
        src: &Window,
        dst: &Window,
        tab: TabId,
        record: serde_json::Value,
        fds: Vec<OwnedFd>,
    ) -> (TabId, serde_json::Value) {
        dst.handle.hold_presence(SESSION, 1);
        assert!(
            dst.heard
                .borrow()
                .iter()
                .any(|e| matches!(e, ChatMoveEvent::PresenceHeld { mv: 1 })),
            "a window with no presence to hold answers at once"
        );
        let arrived = dst
            .handle
            .adopt_moved(ViewId::BASE, &record, fds)
            .expect("the tab is taken up");
        src.handle
            .forget_moved(ViewId::BASE, Some(tab))
            .expect("the source lets it go");
        (arrived, record)
    }

    #[test]
    fn a_moved_tab_keeps_following() {
        for following in [false, true] {
            let (src, dst) = (window(), window());
            let tab = src.handle.state.borrow().tabs.active();
            src.handle.state.borrow_mut().tabs.get_mut(tab).unwrap().following = following;
            dst.handle.set_editor_follow(!following);
            let (arrived, _) = move_tab(&src, &dst, tab);
            assert_eq!(
                dst.handle.state.borrow().tabs.get(arrived).unwrap().following,
                following
            );
            let state = dst.handle.state.borrow();
            let payload: serde_json::Value = serde_json::from_str(&state.tabs.tabs_payload()).unwrap();
            assert_eq!(payload["tabs"][0].get("following").is_some(), following);
        }
    }

    #[test]
    fn an_idle_claude_chat_moves_and_commits_with_no_new_event() {
        let dir = workspace("chat-move-idle");
        let (src, dst) = (window(), window());
        let (tab, provider) = moving_tab(&src, &dir);
        let (arrived, record) = move_tab(&src, &dst, tab);
        assert_eq!(
            provider.steps(),
            vec!["freeze", "adopt", "commit", "forget"],
            "frozen, taken up, committed with nothing new from the agent, let go"
        );
        assert_eq!(record["lease"], true, "the session's lease went with it");
        assert!(
            src.handle.state.borrow().tabs.get(tab).is_none(),
            "the tab left the source"
        );
        assert_eq!(provider.inner.closes(), 0, "no session was ended");
        let state = dst.handle.state.borrow();
        assert_eq!(state.tabs.active(), arrived, "it is the destination's active tab");
        assert_eq!(
            state.tabs.base_tabs(),
            vec![arrived],
            "it took the untouched empty tab's place"
        );
        assert_eq!(
            state.tabs.get(arrived).and_then(Tab::provider_session_id).as_deref(),
            Some(SESSION)
        );
    }

    #[test]
    fn the_moved_projection_serialises_equal_with_its_last_revision() {
        let dir = workspace("chat-move-projection");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        let before = projection_of(&src, tab);
        let (arrived, _) = move_tab(&src, &dst, tab);
        let after = projection_of(&dst, arrived);
        assert_eq!(
            after["last_revision"], before["last_revision"],
            "the source's revision carries over"
        );
        // Field by field, every sequence number with it; only the conversation's own ids are the destination's.
        let fields = before.as_object().unwrap();
        for (name, value) in fields {
            assert_eq!(&after[name], value, "the projection's `{name}` differs after the move");
        }
    }

    #[test]
    fn the_activity_line_shows_running_after_adoption() {
        let dir = workspace("chat-move-running");
        let (src, dst) = (window(), window());
        let (tab, provider) = moving_tab(&src, &dir);
        // A second turn runs when the chat moves.
        stream(&provider.inner, "t2", "still going");
        tick_handle_until(&src.handle, "the second turn", || {
            src.handle.state.borrow().tabs.get(tab).is_some_and(Tab::turn_running)
        });
        let (arrived, _) = move_tab(&src, &dst, tab);
        let projection = projection_of(&dst, arrived);
        assert_eq!(projection["status"]["kind"], "running", "{}", projection["status"]);
        assert_eq!(projection["active_turn_id"], "t2", "the running turn is the moved one");
        assert!(dst
            .handle
            .state
            .borrow()
            .tabs
            .get(arrived)
            .is_some_and(Tab::turn_running));
    }

    #[test]
    fn a_chat_stopped_at_an_unanswered_card_moves_and_the_card_is_answered_in_the_destination() {
        let dir = workspace("chat-move-card");
        let (src, dst) = (window(), window());
        let (tab, provider) = moving_tab(&src, &dir);
        stream(&provider.inner, "t2", "about to write");
        provider.inner.queue(write_card("perm-7"));
        tick_handle_until(&src.handle, "the card", || waiting(&src.handle, tab, "perm-7"));
        let (arrived, _) = move_tab(&src, &dst, tab);
        assert!(
            waiting(&dst.handle, arrived, "perm-7"),
            "the card waits in the destination"
        );
        assert!(provider.inner.decisions().is_empty(), "nobody answered it on the way");
        permission_response(&dst.handle, ViewId::BASE, "answer-1", arrived, "perm-7");
        assert_eq!(
            provider
                .inner
                .decisions()
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["perm-7"],
            "answered once, by the destination, under the number it had"
        );
    }

    #[test]
    fn a_page_that_does_not_answer_moves_the_tab_without_its_view() {
        let dir = workspace("chat-move-no-view");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        src.handle.freeze_for_move(ViewId::BASE, Some(tab), 1);
        assert!(export_asked(&src.page, tab).is_some());
        assert!(
            !src.heard
                .borrow()
                .iter()
                .any(|e| matches!(e, ChatMoveEvent::Frozen { .. })),
            "the freeze waits for the page"
        );
        std::thread::sleep(VIEW_WAIT);
        src.handle.tick();
        let (record, fds) = frozen(&src, 1);
        assert!(record["view"].is_null(), "no view went with it");
        dst.handle.hold_presence(SESSION, 1);
        let arrived = dst
            .handle
            .adopt_moved(ViewId::BASE, &record, fds)
            .expect("taken up all the same");
        assert!(
            !dst.page.sent.borrow().iter().any(|e| kind_of(e) == "view_import"),
            "the destination's page is given no view"
        );
        assert!(dst.handle.state.borrow().tabs.get(arrived).is_some());
    }

    #[test]
    fn an_arrived_tab_reaches_its_page_in_one_send() {
        let dir = workspace("chat-move-batch");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        dst.page.batches.borrow_mut().clear();
        move_tab(&src, &dst, tab);
        let batches = dst.page.batches.borrow();
        let arrival: Vec<_> = batches
            .iter()
            .filter(|batch| batch.iter().any(|p| kind_of(p) == "view_import"))
            .collect();
        assert_eq!(arrival.len(), 1);
        let kinds: Vec<_> = arrival[0].iter().map(|p| kind_of(p)).collect();
        assert_eq!(kinds[0], "view_import");
        let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
        assert!(at("tabs") < at("snapshot"));
        assert!(at("snapshot") < at("draft"));
    }

    #[test]
    fn the_destinations_page_is_given_the_view_before_it_is_shown_the_tab() {
        let dir = workspace("chat-move-view");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        clear(&[&dst.page]);
        let (arrived, record) = move_tab(&src, &dst, tab);
        assert_eq!(record["view"], page_view(), "the page's view went unread");
        let sent: Vec<serde_json::Value> = dst.page.sent.borrow().iter().map(|e| json_of(e)).collect();
        let import = sent
            .iter()
            .position(|v| v["kind"] == "view_import" && v["tab"] == arrived.0)
            .expect("the view is given to the page");
        assert_eq!(sent[import]["view"], page_view());
        let strip = sent
            .iter()
            .position(|v| v["kind"] == "tabs" && v["active"] == arrived.0)
            .expect("the strip shows the tab");
        assert!(import < strip, "kept before the switch shows the tab");
    }

    #[test]
    fn a_view_that_arrives_before_its_page_is_ready_is_given_to_it_at_its_ready() {
        let dir = workspace("chat-move-view-early");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        // The destination's page is still loading its document when the move commits.
        dst.handle.set_document_ready(ViewId::BASE, false);
        clear(&[&dst.page]);
        let (arrived, _) = move_tab(&src, &dst, tab);
        assert!(
            !dst.page.sent.borrow().iter().any(|e| kind_of(e) == "view_import"),
            "nothing is dispatched into a page with no document"
        );
        clear(&[&dst.page]);
        dst.handle.page_message(ViewId::BASE, READY);
        let sent: Vec<serde_json::Value> = dst.page.sent.borrow().iter().map(|e| json_of(e)).collect();
        let import = sent
            .iter()
            .position(|v| v["kind"] == "view_import" && v["tab"] == arrived.0)
            .expect("the view is given to the page at its ready");
        assert_eq!(sent[import]["view"], page_view());
        let strip = sent
            .iter()
            .position(|v| v["kind"] == "tabs" && v["active"] == arrived.0)
            .expect("the strip shows the tab");
        assert!(import < strip, "ahead of the switch that takes it up");
        clear(&[&dst.page]);
        dst.handle.page_message(ViewId::BASE, READY);
        assert!(
            !dst.page.sent.borrow().iter().any(|e| kind_of(e) == "view_import"),
            "given once: a later document does not get it again"
        );
    }

    #[test]
    fn no_end_snapshot_is_taken_in_the_source_and_its_running_file_is_gone_after_move_done() {
        let dir = workspace("chat-move-presence");
        let review = dir.join(".review-state");
        let (src, dst) = (window(), window());
        let pid = 4321;
        src.handle
            .state
            .borrow_mut()
            .tabs
            .hold_presence(eitri_core::turn_review::PresenceHolder::start(review.clone(), pid));
        let (tab, provider) = moving_tab(&src, &dir);
        stream(&provider.inner, "t2", "running");
        let running = review.join(format!("running/{pid}.{}", tab.0));
        tick_handle_until(&src.handle, "the turn's file", || running.exists());
        move_tab(&src, &dst, tab);
        let gone = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while running.exists() {
            assert!(std::time::Instant::now() < gone, "the source's running file stayed");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(provider.inner.closes(), 0, "the moved session was not ended");
        assert!(
            src.handle.state.borrow().retiring.is_empty(),
            "nothing of it is counted as retiring"
        );
    }

    #[test]
    fn a_freeze_refused_by_the_agent_leaves_the_tab_as_it_was() {
        let dir = workspace("chat-move-refused");
        let src = window();
        let (tab, provider) = moving_tab(&src, &dir);
        provider.refuse_freezes(Some("a message sent into the turn has not been read yet"));
        src.handle.freeze_for_move(ViewId::BASE, Some(tab), 3);
        let request = export_asked(&src.page, tab).unwrap();
        answer_export(&src, request, serde_json::Value::Null);
        heard_back(&src);
        let heard = src.heard.borrow();
        assert!(
            matches!(&heard[..], [ChatMoveEvent::FreezeRefused { mv: 3, why, .. }] if why.contains("not been read")),
            "{heard:?}"
        );
        drop(heard);
        assert!(
            !src.handle.state.borrow().tabs.is_moving(tab),
            "the tab takes input again"
        );
        assert!(
            src.handle.chat_move_facts(ViewId::BASE, None).is_ok(),
            "and may be asked to move again"
        );
    }

    #[test]
    fn a_move_called_off_takes_the_tab_and_its_agent_back() {
        let dir = workspace("chat-move-back");
        let src = window();
        let (tab, provider) = moving_tab(&src, &dir);
        src.handle.freeze_for_move(ViewId::BASE, Some(tab), 1);
        answer_export(&src, export_asked(&src.page, tab).unwrap(), page_view());
        let _sent = frozen(&src, 1);
        assert!(src.handle.state.borrow().tabs.is_moving(tab));
        assert!(
            src.handle.chat_move_facts(ViewId::BASE, None).is_err(),
            "already moving"
        );
        src.handle.resume_moved(ViewId::BASE, None);
        assert_eq!(provider.steps(), vec!["freeze", "resume"]);
        assert!(!src.handle.state.borrow().tabs.is_moving(tab));
        assert!(src.handle.state.borrow().tabs.get(tab).is_some(), "the tab stayed");
        assert!(
            src.handle.forget_moved(ViewId::BASE, Some(tab)).is_err(),
            "a tab taken back cannot be let go"
        );
    }

    #[test]
    fn a_moved_record_that_is_not_one_is_refused_whole() {
        let dir = workspace("chat-move-bad-record");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        src.handle.freeze_for_move(ViewId::BASE, Some(tab), 1);
        answer_export(&src, export_asked(&src.page, tab).unwrap(), page_view());
        let (record, fds) = frozen(&src, 1);
        let before = dst.handle.state.borrow().tabs.tabs().len();
        let pipe = || -> OwnedFd { std::io::pipe().unwrap().0.into() };
        let mut cases: Vec<(serde_json::Value, Vec<OwnedFd>, &str)> = Vec::new();
        let mut versioned = record.clone();
        versioned["version"] = 2.into();
        cases.push((versioned, (0..4).map(|_| pipe()).collect(), "version"));
        cases.push((record.clone(), (0..2).map(|_| pipe()).collect(), "descriptors"));
        let mut foreign = record.clone();
        foreign["provider"]["kind"] = "gemini".into();
        cases.push((foreign, (0..4).map(|_| pipe()).collect(), "no agent"));
        let mut no_chat = record.clone();
        no_chat["chat"] = serde_json::Value::Null;
        no_chat["provider"] = serde_json::Value::Null;
        cases.push((no_chat, (0..1).map(|_| pipe()).collect(), "do not agree"));
        cases.push((serde_json::json!([1, 2]), Vec::new(), "not an object"));
        for (bad, fds, says) in cases {
            let why = dst.handle.adopt_moved(ViewId::BASE, &bad, fds).expect_err("refused");
            assert!(why.contains(says), "{why:?} does not say {says:?}");
        }
        assert_eq!(
            dst.handle.state.borrow().tabs.tabs().len(),
            before,
            "nothing was taken up"
        );
        drop(fds);
    }

    #[test]
    fn the_read_names_the_tab_its_session_and_whether_it_moves_live() {
        let dir = workspace("chat-move-read");
        let src = window();
        let (tab, provider) = moving_tab(&src, &dir);
        let facts = src.handle.chat_move_facts(ViewId::BASE, None).unwrap();
        assert_eq!(
            facts,
            eitri_core::wm::ChatMoveFacts {
                tab,
                session: Some(SESSION.into()),
                live: true,
                running: false,
            }
        );
        assert_eq!(src.handle.view_of_session(SESSION), Some(ViewId::BASE));
        assert_eq!(src.handle.view_of_session("nobody's"), None);
        assert_eq!(
            src.handle.chat_move_facts(ViewId::BASE, Some(SESSION)).map(|f| f.tab),
            Ok(tab),
            "a base view's tab by its session"
        );
        assert!(src.handle.chat_move_facts(ViewId::BASE, Some("nobody's")).is_err());
        assert!(
            src.handle.chat_move_facts(view(2), None).is_err(),
            "a view with no tab moves nothing"
        );
        drop(provider);
    }

    const STATE_TOOL: &str = "editor_state";
    const OPEN_TOOL: &str = "open_file";

    fn declared_tools() -> agent::ClientToolSet {
        let declared = |name: &str| agent::ClientToolDecl {
            name: name.into(),
            description: "reads the editor".into(),
            input_schema_json: r#"{"type":"object"}"#.into(),
            always_load: true,
        };
        agent::ClientToolSet {
            server_name: eitri_core::editor_tools::SERVER_NAME.into(),
            timeout_ms: eitri_core::editor_tools::CALL_TIMEOUT_MS,
            tools: vec![declared(STATE_TOOL), declared(OPEN_TOOL)],
        }
    }

    /// A pair of windows with an editor each, and in the source a chat given the editor tools, in the middle of a turn.
    struct Pair {
        src: Window,
        dst: Window,
        src_editor: Rc<FakeEditor>,
        dst_editor: Rc<FakeEditor>,
        provider: Arc<MovingProvider>,
        tab: TabId,
        /// A file outside the project: a call for it raises a card.
        outside: PathBuf,
    }

    fn window_with_editor(dir: &Path) -> (Window, Rc<FakeEditor>) {
        let w = window();
        let editor = Rc::new(FakeEditor::default());
        w.handle.set_editor_rpc(editor.clone());
        w.handle.state.borrow_mut().project_dir = dir.to_path_buf();
        (w, editor)
    }

    fn pair(label: &str) -> Pair {
        let dir = workspace(label);
        let (src, src_editor) = window_with_editor(&dir);
        let (dst, dst_editor) = window_with_editor(&dir);
        let provider = RecordingProvider::client_tooling();
        let (tab, provider) = session_tab(&src, &dir, provider, Some(declared_tools()));
        stream(&provider.inner, "t2", "working");
        tick_handle_until(&src.handle, "the second turn", || {
            src.handle.state.borrow().tabs.get(tab).is_some_and(Tab::turn_running)
        });
        let outside = workspace(&format!("{label}-outside")).join("notes.txt");
        std::fs::write(&outside, "notes\n").unwrap();
        Pair {
            src,
            dst,
            src_editor,
            dst_editor,
            provider,
            tab,
            outside,
        }
    }

    fn call_at(id: &str, sequence: u64, tool: &str, arguments: String) -> agent::AgentDomainEvent {
        agent::AgentDomainEvent::ClientToolCall {
            call_id: id.into(),
            tool_use_id: Some(format!("toolu-{id}")),
            tool: tool.into(),
            arguments_json: arguments,
            sequence,
        }
    }

    fn state_call_at(id: &str, sequence: u64) -> agent::AgentDomainEvent {
        call_at(id, sequence, STATE_TOOL, "{}".into())
    }

    fn open_refs(ids: &[&str]) -> Vec<agent::ClientCallRef> {
        ids.iter()
            .map(|id| agent::ClientCallRef {
                call_id: (*id).into(),
                tool_use_id: None,
                tool: STATE_TOOL.into(),
            })
            .collect()
    }

    /// The call ids that reached the agent, in order.
    fn answered(provider: &MovingProvider) -> Vec<String> {
        provider.inner.client_answers().into_iter().map(|a| a.call_id).collect()
    }

    fn assert_told_to_try_again(answer: &agent::ClientToolAnswer) {
        assert!(answer.is_error, "{answer:?}");
        assert_eq!(answer.text, [eitri_core::editor_tools::words::MOVED]);
    }

    fn open_in(w: &Window, tab: TabId) -> Vec<String> {
        let state = w.handle.state.borrow();
        let backend = state.tabs.get(tab).and_then(Tab::live).expect("a live tab");
        let open = backend.open_client_calls();
        open.into_iter().map(|call| call.call_id).collect()
    }

    fn reads(w: &Window) -> usize {
        w.handle.state.borrow().client_call_reads.len()
    }

    fn read_done(w: &Window) {
        tick_handle_until(&w.handle, "the read of the open calls", || reads(w) == 0);
    }

    fn settle(w: &Window) {
        for _ in 0..4 {
            w.handle.tick();
        }
    }

    /// A call for the file outside the project, as far as its card: returns the card's id.
    fn raise_a_card(pair: &Pair, id: &str, sequence: u64) -> String {
        let arguments = serde_json::json!({ "path": pair.outside }).to_string();
        pair.provider.inner.queue(call_at(id, sequence, OPEN_TOOL, arguments));
        let cards = || pair.src.handle.state.borrow().editor_tools.cards(pair.tab);
        tick_handle_until(&pair.src.handle, "the card", || !cards().is_empty());
        cards()[0].id.clone()
    }

    /// Waits, ticking nothing, until `id` has folded and the ingest has put its batch down: the next freeze is taken
    /// at once, and it is that freeze's last pump that finds the call.
    fn wait_for_the_fold(w: &Window, tab: TabId, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let folded = open_in(w, tab).iter().any(|call| call == id);
            let mut state = w.handle.state.borrow_mut();
            let backend = state.tabs.get_mut(tab).and_then(Tab::live_mut).expect("a live tab");
            if folded && backend.freeze_folding().is_ok() {
                backend.unfreeze_folding();
                return;
            }
            drop(state);
            assert!(Instant::now() < deadline, "{id} never folded");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Freezes the tab for move 1 with no tick in between, so no ordinary pump can reach what the last pump finds. A
    /// freeze asked again is asked here, not on a tick.
    fn freeze_with_no_tick(w: &Window, tab: TabId) -> (serde_json::Value, Vec<OwnedFd>) {
        w.handle.freeze_for_move(ViewId::BASE, Some(tab), 1);
        answer_export(w, export_asked(&w.page, tab).unwrap(), page_view());
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.heard.borrow().is_empty() {
            assert!(Instant::now() < deadline, "the freeze never answered");
            w.handle.run(poll_chat_moves);
            std::thread::sleep(Duration::from_millis(1));
        }
        frozen(w, 1)
    }

    /// A pair whose tab has a bridge socket, and the bridge connected to it.
    fn bridged_pair(label: &str) -> (Pair, agent::EditorServerSpec, Peer) {
        let pair = pair(label);
        let dir = pair.src.handle.state.borrow().project_dir.clone();
        let server = serve(&pair.src.handle, pair.tab, &dir);
        let (peer, _) = Peer::connect(&server);
        (pair, server, peer)
    }

    fn freeze_for_move_one(pair: &Pair) -> (serde_json::Value, Vec<OwnedFd>) {
        pair.src.handle.freeze_for_move(ViewId::BASE, Some(pair.tab), 1);
        answer_export(&pair.src, export_asked(&pair.src.page, pair.tab).unwrap(), page_view());
        frozen(&pair.src, 1)
    }

    fn nothing_open(pair: &Pair) {
        pair.provider.inner.serve_client_state(agent::ClientToolCallState {
            through_sequence: 3,
            open: Vec::new(),
        });
    }

    /// The agent's tool server stays where it is and was told one path and one key, so both hold in the other window.
    #[test]
    fn a_moved_tab_keeps_its_socket_name_and_key() {
        let (pair, server, mut peer) = bridged_pair("move-bridge-name");
        nothing_open(&pair);
        let (arrived, record) = move_tab(&pair.src, &pair.dst, pair.tab);
        assert!(peer.is_ended(), "the source let the socket go");
        let bridge = &record["panel"]["editor_bridge"];
        assert_eq!(bridge["name"]["key"], server.args[1].as_str());
        assert!(server.args[0].contains(bridge["name"]["hex"].as_str().unwrap()));

        let (mut moved, _) = Peer::connect(&server);
        moved.call(1, STATE_TOOL, serde_json::json!({}));
        tick_handle_until(&pair.dst.handle, "the call to reach the destination's editor", || {
            pair.dst_editor.asked().len() == 1
        });
        pair.dst_editor.answer_oldest(&state_reply());
        tick_handle_until(&pair.dst.handle, "the broker to finish", || {
            pair.dst.handle.state.borrow().editor_tools.is_idle()
        });
        assert!(matches!(
            moved.hear(),
            eitri_core::editor_tools::bridge_wire::Message::Result {
                id: 1,
                is_error: false,
                ..
            }
        ));
        assert!(pair.src.handle.state.borrow().editor_bridge_tabs.is_empty());
        assert!(pair.dst.handle.state.borrow().editor_bridge_tabs.contains_key(&arrived));
    }

    /// The list is the session's: the other window's switches say what a session started there gets.
    #[test]
    fn a_moved_tab_keeps_what_it_was_declared() {
        let (pair, server, _peer) = bridged_pair("move-bridge-declared");
        nothing_open(&pair);
        let given = pair.src.handle.state.borrow().editor_bridge_tabs[&pair.tab]
            .tools
            .clone();
        assert!(!given.is_empty());
        pair.dst.handle.state.borrow_mut().editor_switches =
            eitri_core::editor_tools::switches::Switches::parse(Some("off"), &[]).unwrap();

        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        let declared = pair.dst.handle.state.borrow().tabs.editor_declared(arrived).cloned();
        assert_eq!(declared, Some(given.iter().cloned().collect()));
        let (_moved, served) = Peer::connect(&server);
        assert_eq!(served, given);
    }

    #[test]
    fn a_bridge_call_open_across_a_move_is_answered_once() {
        let (pair, _server, mut peer) = bridged_pair("move-bridge-call");
        peer.call(1, STATE_TOOL, serde_json::json!({}));
        tick_handle_until(&pair.src.handle, "the call to reach the editor", || {
            pair.src_editor.asked().len() == 1
        });
        nothing_open(&pair);

        move_tab(&pair.src, &pair.dst, pair.tab);
        pair.src_editor.answer_oldest(&state_reply());
        settle(&pair.src);
        settle(&pair.dst);

        let eitri_core::editor_tools::bridge_wire::Message::Result { id, text, is_error } = peer.hear() else {
            panic!("a result")
        };
        assert_eq!(id, 1);
        assert!(is_error);
        assert_eq!(text, eitri_core::editor_tools::words::MOVED);
        assert!(peer.is_ended(), "and nothing after it");
        assert!(
            pair.dst_editor.asked().is_empty(),
            "a call answered there does not run here"
        );
    }

    #[test]
    fn a_move_called_off_binds_again() {
        let (pair, server, mut peer) = bridged_pair("move-bridge-off");
        let _sent = freeze_for_move_one(&pair);
        assert!(peer.is_ended());
        assert!(
            std::os::unix::net::UnixStream::connect(&server.args[0]).is_err(),
            "the name is free for the other window"
        );

        pair.src.handle.resume_moved(ViewId::BASE, None);
        let (mut again, _) = Peer::connect(&server);
        again.call(1, STATE_TOOL, serde_json::json!({}));
        tick_handle_until(&pair.src.handle, "the call to reach the editor", || {
            pair.src_editor.asked().len() == 1
        });
    }

    /// The session stays here, and the server it runs has nowhere else to go.
    #[test]
    fn a_refused_provider_freeze_keeps_the_socket() {
        let (pair, server, mut peer) = bridged_pair("move-bridge-refused");
        pair.provider
            .refuse_freezes(Some("a message sent into the turn has not been read yet"));
        pair.src.handle.freeze_for_move(ViewId::BASE, Some(pair.tab), 1);
        answer_export(&pair.src, export_asked(&pair.src.page, pair.tab).unwrap(), page_view());
        heard_back(&pair.src);
        assert!(!pair.src.handle.state.borrow().tabs.is_moving(pair.tab));

        peer.call(1, STATE_TOOL, serde_json::json!({}));
        tick_handle_until(&pair.src.handle, "the call to reach the editor", || {
            pair.src_editor.asked().len() == 1
        });
        pair.src_editor.answer_oldest(&state_reply());
        tick_handle_until(&pair.src.handle, "the broker to finish", || {
            pair.src.handle.state.borrow().editor_tools.is_idle()
        });
        assert!(matches!(
            peer.hear(),
            eitri_core::editor_tools::bridge_wire::Message::Result {
                id: 1,
                is_error: false,
                ..
            }
        ));
        let (_second, _) = Peer::connect(&server);
    }

    #[test]
    fn a_call_open_across_a_move_is_answered_once_by_the_source() {
        let pair = pair("move-call-source");
        pair.provider.inner.queue(state_call_at("c1", 3));
        tick_handle_until(&pair.src.handle, "the call to reach the editor", || {
            pair.src_editor.asked().len() == 1
        });
        pair.provider.inner.serve_client_state(agent::ClientToolCallState {
            through_sequence: 3,
            open: Vec::new(),
        });

        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        read_done(&pair.dst);
        pair.src_editor.answer_oldest(&state_reply());
        settle(&pair.src);
        settle(&pair.dst);

        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c1"]);
        assert_told_to_try_again(&answers[0]);
        assert_eq!(
            open_in(&pair.dst, arrived),
            ["c1"],
            "its settled event has not come yet"
        );
        assert!(
            pair.dst_editor.asked().is_empty(),
            "a call answered there does not run here"
        );
    }

    #[test]
    fn a_card_goes_with_the_freeze() {
        let pair = pair("move-card-goes");
        let card = raise_a_card(&pair, "c1", 3);
        assert!(pair.provider.inner.client_answers().is_empty());

        let _sent = {
            pair.src.handle.freeze_for_move(ViewId::BASE, Some(pair.tab), 1);
            answer_export(&pair.src, export_asked(&pair.src.page, pair.tab).unwrap(), page_view());
            frozen(&pair.src, 1)
        };

        let state = pair.src.handle.state.borrow();
        assert!(
            state.editor_tools.cards(pair.tab).is_empty(),
            "the broker holds it no more"
        );
        assert!(state.editor_tools.is_idle());
        assert_eq!(state.tabs.get(pair.tab).unwrap().attention.attention().pending, 0);
        drop(state);
        let newest = pair
            .src
            .page
            .sent
            .borrow()
            .iter()
            .rev()
            .map(|envelope| json_of(envelope))
            .find(|value| value["kind"] == "editor_cards" && value["tab"] == pair.tab.0)
            .expect("the page is told");
        assert_eq!(newest["cards"], serde_json::json!([]));

        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c1"]);
        assert_told_to_try_again(&answers[0]);
        let late = serde_json::json!({
            "type": "editor_card_answer", "request_id": "card-1", "tab": pair.tab.0, "card_id": card,
            "decision": "allow", "origin": "key",
        });
        pair.src.handle.page_message(ViewId::BASE, &late.to_string());
        settle(&pair.src);
        assert!(pair.src_editor.asked().is_empty(), "a key on its way meets no card");
    }

    #[test]
    fn the_destination_has_no_card_and_is_not_blocked() {
        let pair = pair("move-no-card");
        raise_a_card(&pair, "c1", 3);
        pair.provider.inner.serve_client_state(agent::ClientToolCallState {
            through_sequence: 3,
            open: Vec::new(),
        });
        clear(&[&pair.dst.page]);

        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        read_done(&pair.dst);

        let snapshot = pair
            .dst
            .page
            .sent
            .borrow()
            .iter()
            .rev()
            .map(|envelope| json_of(envelope))
            .find(|value| value["kind"] == "snapshot" && value["tab"] == arrived.0)
            .expect("a snapshot of the arrived tab");
        assert!(snapshot["state"].get("editorCards").is_none(), "{snapshot}");
        let state = pair.dst.handle.state.borrow();
        assert_eq!(state.tabs.get(arrived).unwrap().attention.attention().pending, 0);
        assert!(state.editor_tools.cards(arrived).is_empty());
        drop(state);

        pair.provider.inner.queue(state_call_at("c2", 4));
        tick_handle_until(&pair.dst.handle, "the next call to run", || {
            pair.dst_editor.asked().len() == 1
        });
    }

    /// The events behind a read reach the window after it, so a call the read counted can fold after the release.
    #[test]
    fn the_destination_answers_what_is_still_open_and_runs_nothing_at_or_below_the_snapshot() {
        let pair = pair("move-snapshot");
        pair.provider.inner.serve_client_state(agent::ClientToolCallState {
            through_sequence: 6,
            open: open_refs(&["c1", "c2"]),
        });

        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        read_done(&pair.dst);

        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c1", "c2"]);
        answers.iter().for_each(assert_told_to_try_again);

        pair.provider.inner.queue_all(vec![
            state_call_at("c1", 4),
            state_call_at("c2", 5),
            state_call_at("settled-since", 6),
        ]);
        tick_handle_until(&pair.dst.handle, "the calls behind the read", || {
            open_in(&pair.dst, arrived).len() == 3
        });
        settle(&pair.dst);
        assert!(
            pair.dst_editor.asked().is_empty(),
            "nothing at or below the snapshot runs"
        );
        assert!(pair.dst.handle.state.borrow().editor_tools.is_idle());
        assert_eq!(pair.provider.inner.client_answers().len(), 2, "each told once");
    }

    /// A call that folds during the hold raises no wake once the hold ends, so the release itself has to report it.
    #[test]
    fn a_call_after_the_snapshot_runs_at_the_destination() {
        let pair = pair("move-after-snapshot");
        pair.provider.inner.hold_client_reads(true);
        pair.provider.inner.serve_client_state(agent::ClientToolCallState {
            through_sequence: 3,
            open: Vec::new(),
        });
        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        assert_eq!(reads(&pair.dst), 1);

        pair.provider.inner.queue(state_call_at("c2", 4));
        tick_handle_until(&pair.dst.handle, "the call to fold", || {
            open_in(&pair.dst, arrived) == ["c2"]
        });
        settle(&pair.dst);
        assert!(pair.dst_editor.asked().is_empty(), "held until the session has said");

        pair.provider.inner.reply_to_held_client_reads();
        pair.dst.handle.tick();
        assert_eq!(
            pair.dst_editor.asked(),
            [eitri_core::editor_tools::tools::editor_state::EDITOR_STATE_LUA]
        );
        pair.dst_editor.answer_oldest(&state_reply());
        tick_handle_until(&pair.dst.handle, "the answer", || {
            !pair.provider.inner.client_answers().is_empty()
        });
        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c2"]);
        assert!(!answers[0].is_error, "{:?}", answers[0].text);
    }

    #[test]
    fn a_failed_read_runs_nothing_at_or_below_folded_through_and_runs_later_calls() {
        let pair = pair("move-failed-read");
        pair.provider.report_position(3);
        pair.provider.inner.queue(state_call_at("c1", 3));
        tick_handle_until(&pair.src.handle, "the call to reach the editor", || {
            pair.src_editor.asked().len() == 1
        });

        let (arrived, record) = move_tab(&pair.src, &pair.dst, pair.tab);
        assert_eq!(record["chat"]["core"]["folded_through"], 3);
        read_done(&pair.dst);
        assert_eq!(answered(&pair.provider), ["c1"], "told once, by the source");

        pair.provider
            .inner
            .queue_all(vec![state_call_at("behind", 2), state_call_at("c2", 4)]);
        tick_handle_until(&pair.dst.handle, "the later call to run", || {
            pair.dst_editor.asked().len() == 1
        });
        assert!(open_in(&pair.dst, arrived).contains(&"behind".to_string()));
        settle(&pair.dst);
        assert_eq!(pair.dst_editor.asked().len(), 1, "only the later call ran");
    }

    /// A read can fail with the session alive, and then no second window asks: a call the broker never heard of has
    /// its one answer from the window that found it, or none until the agent's tool timeout.
    #[test]
    fn a_failed_read_leaves_no_call_of_the_last_pump_waiting() {
        let pair = pair("move-failed-read-last-pump");
        pair.provider.report_position(3);
        pair.provider.inner.queue(state_call_at("c1", 3));
        wait_for_the_fold(&pair.src, pair.tab, "c1");

        let (record, fds) = freeze_with_no_tick(&pair.src, pair.tab);
        land(&pair.src, &pair.dst, pair.tab, record, fds);
        read_done(&pair.dst);
        settle(&pair.dst);

        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c1"]);
        assert_told_to_try_again(&answers[0]);
        assert!(pair.dst_editor.asked().is_empty());
    }

    /// A window that holds a tab's calls has reported none of them, so nothing but its leaving can answer one that
    /// folded during the hold when the next window's read fails.
    #[test]
    fn a_tab_that_leaves_during_its_read_tells_the_calls_it_held() {
        let pair = pair("move-held-read");
        pair.provider.inner.hold_client_reads(true);
        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        pair.provider.report_position(3);
        pair.provider.inner.queue(state_call_at("c1", 3));
        tick_handle_until(&pair.dst.handle, "the call to fold during the hold", || {
            open_in(&pair.dst, arrived).contains(&"c1".to_string())
        });
        assert!(
            pair.provider.inner.client_answers().is_empty(),
            "the read is waited for"
        );

        pair.provider.inner.hold_client_reads(false);
        move_tab(&pair.dst, &pair.src, arrived);
        read_done(&pair.src);

        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c1"]);
        assert_told_to_try_again(&answers[0]);
        assert!(pair.dst_editor.asked().is_empty());
        assert!(pair.src_editor.asked().is_empty());
    }

    #[test]
    fn a_call_opened_by_the_last_pump_is_not_run_by_the_source() {
        let pair = pair("move-last-pump");
        pair.provider.inner.queue(state_call_at("c1", 3));
        wait_for_the_fold(&pair.src, pair.tab, "c1");
        pair.provider.inner.serve_client_state(agent::ClientToolCallState {
            through_sequence: 3,
            open: Vec::new(),
        });

        let (record, fds) = freeze_with_no_tick(&pair.src, pair.tab);
        settle(&pair.src);
        assert!(pair.src_editor.asked().is_empty());
        assert!(pair.src.handle.state.borrow().editor_tools.is_idle());
        let answers = pair.provider.inner.client_answers();
        assert_eq!(
            answered(&pair.provider),
            ["c1"],
            "the source answers what it found last"
        );
        assert_told_to_try_again(&answers[0]);

        land(&pair.src, &pair.dst, pair.tab, record, fds);
        read_done(&pair.dst);
        settle(&pair.dst);
        assert_eq!(answered(&pair.provider), ["c1"], "told once");
        assert!(pair.dst_editor.asked().is_empty());
    }

    /// The call the broker held was told to try again and stays told. The one the last pump found was told nothing,
    /// so it runs, and later calls run as ever.
    #[test]
    fn a_move_called_off_loses_nothing_but_the_open_calls() {
        let pair = pair("move-called-off");
        pair.provider.inner.queue(state_call_at("c1", 3));
        tick_handle_until(&pair.src.handle, "the call to reach the editor", || {
            pair.src_editor.asked().len() == 1
        });
        pair.provider.inner.queue(state_call_at("c2", 4));
        wait_for_the_fold(&pair.src, pair.tab, "c2");

        let _sent = freeze_with_no_tick(&pair.src, pair.tab);
        pair.src.handle.resume_moved(ViewId::BASE, None);
        assert_eq!(pair.provider.steps().last(), Some(&"resume"));

        pair.src_editor.answer_oldest(&state_reply());
        settle(&pair.src);
        let answers = pair.provider.inner.client_answers();
        assert_eq!(answered(&pair.provider), ["c1", "c2"], "each answered once");
        answers.iter().for_each(assert_told_to_try_again);
        assert_eq!(
            pair.src_editor.asked().len(),
            1,
            "a call told to try again does not run"
        );

        pair.provider.inner.queue(state_call_at("c3", 5));
        tick_handle_until(&pair.src.handle, "a later call", || pair.src_editor.asked().len() == 2);
        let state = pair.src.handle.state.borrow();
        assert!(
            state.tabs.get(pair.tab).is_some_and(Tab::turn_running),
            "the turn went on"
        );
    }

    /// A closed session leaves a read that nothing will ever answer, and the steady tick must not wait for it.
    #[test]
    fn a_closed_session_ends_the_wait() {
        let pair = pair("move-closed");
        pair.provider.inner.hold_client_reads(true);
        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        assert_eq!(reads(&pair.dst), 1);
        settle(&pair.dst);
        assert_eq!(reads(&pair.dst), 1, "the read is waited for");
        assert!(crate::agent_panel::tick::needs_polling(&pair.dst.handle.state.borrow()));

        pair.provider.inner.drop_held_client_reads();
        read_done(&pair.dst);
        assert_eq!(pair.dst.handle.state.borrow().tabs.held_client_calls(arrived), None);
    }

    #[test]
    fn a_tab_that_goes_during_the_read_ends_the_wait() {
        let pair = pair("move-tab-gone");
        pair.provider.inner.hold_client_reads(true);
        let (arrived, _) = move_tab(&pair.src, &pair.dst, pair.tab);
        {
            let mut state = pair.dst.handle.state.borrow_mut();
            let state = &mut *state;
            let removed = state.tabs.remove(arrived).expect("the tab was there");
            let _owed = state.retiring.tab(removed);
        }
        read_done(&pair.dst);
        pair.provider.inner.drop_held_client_reads();
    }

    /// The read is one RPC more on a commit, and an older sidecar has no such call.
    #[test]
    fn a_sidecar_with_no_client_tools_is_neither_read_nor_held() {
        let dir = workspace("move-no-tools");
        let (src, dst) = (window(), window());
        let (tab, _provider) = moving_tab(&src, &dir);
        let (arrived, _) = move_tab(&src, &dst, tab);
        assert_eq!(reads(&dst), 0);
        assert_eq!(dst.handle.state.borrow().tabs.held_client_calls(arrived), None);
    }

    /// Rows as they leave the window, and the freezes the window hears, in one order.
    #[derive(Clone)]
    struct Order(Arc<std::sync::Mutex<Vec<String>>>);

    impl crate::agents_glue::RowSink for Order {
        fn offer(&self, _: Vec<eitri_core::agents::Row>) {
            self.0.lock().unwrap().push("offer".into());
        }
        fn offer_now(&self, rows: Vec<eitri_core::agents::Row>) {
            let moving: Vec<_> = rows.iter().filter(|r| r.moving).map(|r| r.key.clone()).collect();
            self.0.lock().unwrap().push(format!("moving {moving:?}"));
        }
        fn departing(&self, key: String, _: eitri_core::agents::GoneWhy) {
            self.0.lock().unwrap().push(format!("departing {key}"));
        }
    }

    #[test]
    fn a_frozen_tabs_row_goes_out_as_moving_before_the_window_hears_the_freeze() {
        let dir = workspace("chat-move-agents-row");
        let w = window();
        let (tab, _provider) = moving_tab(&w, &dir);
        let key = w
            .handle
            .state
            .borrow()
            .tabs
            .get(tab)
            .unwrap()
            .row_key()
            .as_str()
            .to_owned();
        let order = Order(Arc::default());
        let context = eitri_core::tab_set::RowContext {
            instance: "0123456789abcdef".into(),
            project: eitri_core::agents::ProjectRef {
                key: "fedcba9876543210".into(),
                name: "eitri".into(),
            },
            window: eitri_core::agents::WindowRef {
                instance: "0123456789abcdef".into(),
                name: "1".into(),
                focused: false,
            },
        };
        let old = w.handle.set_agents(Some(crate::agents_glue::AgentsGlue::new(
            context,
            Box::new(order.clone()),
        )));
        assert!(old.is_none());
        w.handle.tick();
        assert_eq!(
            order.0.lock().unwrap().as_slice(),
            ["offer"],
            "the first tick hands the rows on"
        );
        {
            let order = order.clone();
            w.handle.on_chat_move(move |event| {
                if matches!(event, ChatMoveEvent::Frozen { .. }) {
                    order.0.lock().unwrap().push("frozen".into());
                }
            });
        }

        w.handle.freeze_for_move(ViewId::BASE, Some(tab), 1);
        let request = export_asked(&w.page, tab).expect("the page is asked for its view of the tab");
        answer_export(&w, request, page_view());
        tick_handle_until(&w.handle, "the freeze's answer", || {
            order.0.lock().unwrap().iter().any(|seen| seen == "frozen")
        });
        let order = order.0.lock().unwrap().clone();
        let moving = order.iter().position(|seen| *seen == format!("moving {:?}", [&key]));
        let frozen = order.iter().position(|seen| seen == "frozen");
        assert!(
            moving.is_some() && moving < frozen,
            "the moving row was handed on before the hand-over could start: {order:?}"
        );
    }
}
