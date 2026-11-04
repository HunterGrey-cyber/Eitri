//! Moving a module to the other window of the linked pair, for both ends, in the controller.
//!
//! `core::window_link::moves::Moves` is the one judge of where a move stands: every fact that may change it (an
//! answer from the other window, the terminal's freeze, the link's end, the deadline) goes through its method, one
//! transition per fact, and this module only carries out what each answer means. The source keeps the module where it
//! is, on screen, until the destination's `move_done`; then the module's leaf stays as an away placeholder in every
//! workspace and the local copy is dropped. The destination lays the module out without revealing it before anything
//! of it is frozen, holds what `move` brought unread, and builds the module from it only once its `move_done` is known
//! to be on the socket, so a module is never held by both windows nor by neither.
//!
//! The shell does only the per-kind surface work (`PrepareSurface`, `Terminal(Detach)`, `Chat(Freeze)`, `AdoptMoved`,
//! ...): the steps that differ by kind are each kind's op, chosen in one place ([`freeze_op`], [`resume_op`],
//! [`forget_op`]), and a chat's own steps (the base view's tab, the review hold, the presence before `move_done`, a
//! move waiting for its turn's end) are in `chat`. A plugin page carries its saved state and guarded tasks;
//! a Lua panel reopens from the destination's producer, without a surface transfer.

use super::chrome;
use super::feedback;
use super::focus::{self, arrive_tail};
use super::host::*;
use super::module::{SavedEntry, SurfaceDecl};
use super::toasts::ToastKind;
use super::verbs::modules::Cx;
use super::window::{ActivationToken, EditorPresence, LinkSide, PeerId, WinId, Window, WindowRole};
use crate::layout::rules::OpenPoint;
use crate::layout::{declared_placement, open_by_rule, FloatRect, ModuleId, Placement, DEFAULT_FLOAT_SHARE};
use crate::tabs::{TabId, ViewId};
use crate::window_link::ids::arrival_id;
use crate::window_link::moves::{MoveId, Moves, PeerAbort, Role, SourceVerdict, Window as Side, MOVE_DEADLINE};
use crate::window_link::names::WindowName;
use crate::window_link::{FocusTarget, Msg, Shape, Summary};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::rc::Rc;
use std::time::Instant;

/// Where a module is asked to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTarget {
    /// The other window of the linked pair, in its own process.
    Peer(PeerId),
    /// Another window of this process.
    Local(WinId),
}

/// The kinds of module a move knows; the wire name is `move_offer`'s `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Terminal,
    Chat,
    PluginPage,
    LuaPanel,
}

impl MoveKind {
    /// The name `move_offer` carries.
    pub fn wire(self) -> &'static str {
        match self {
            MoveKind::Terminal => "terminal",
            MoveKind::Chat => "chat",
            MoveKind::PluginPage => "plugin_page",
            MoveKind::LuaPanel => "lua_panel",
        }
    }

    /// The kind `move_offer` names; `None` for a name this build does not know.
    pub fn from_wire(name: &str) -> Option<MoveKind> {
        match name {
            "terminal" => Some(MoveKind::Terminal),
            "chat" => Some(MoveKind::Chat),
            "plugin_page" => Some(MoveKind::PluginPage),
            "lua_panel" => Some(MoveKind::LuaPanel),
            _ => None,
        }
    }

    /// The kind of a module declared with `surface`; the editor has none, since it never moves.
    pub fn of(surface: &SurfaceKind) -> Option<MoveKind> {
        match surface {
            SurfaceKind::Editor { .. } => None,
            SurfaceKind::Chat { .. } => Some(MoveKind::Chat),
            SurfaceKind::LuaPanel { .. } => Some(MoveKind::LuaPanel),
            SurfaceKind::PluginPage { .. } => Some(MoveKind::PluginPage),
            SurfaceKind::Terminal { .. } => Some(MoveKind::Terminal),
        }
    }
}

/// What a move of one module comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovePlan {
    /// The linked window: the link's commit machine carries it, and this window keeps an away placeholder.
    Peer {
        module: ModuleId,
        peer: PeerId,
        kind: MoveKind,
    },
    /// The module is away here, held by the linked window: that window is asked to offer it back, since only the
    /// holder can freeze what it holds.
    Ask {
        module: ModuleId,
        peer: PeerId,
    },
    Refused {
        why: &'static str,
    },
}

/// Whether `module` of window `w` may go to `target`, and as what. A chat's tab is read further before it is
/// offered ([`HostReads::chat_move`]); a page's built state decides whether anything needs freezing. A module away
/// here is the other window's to offer, whatever its kind: that window decides whether it can go.
pub fn plan(w: &Window, module: &ModuleId, target: &MoveTarget) -> MovePlan {
    let peer = match target {
        MoveTarget::Local(_) => {
            return MovePlan::Refused {
                why: feedback::LOCAL_MOVE_TEXT,
            }
        }
        MoveTarget::Peer(peer) => *peer,
    };
    if *module == ModuleId::editor() {
        return MovePlan::Refused {
            why: feedback::EDITOR_MOVE_TEXT,
        };
    }
    let Ok(layout) = w.layout.try_borrow() else {
        return MovePlan::Refused {
            why: feedback::LAYOUT_CHANGING_TEXT,
        };
    };
    if !layout.contains(module) && !w.lifecycle.deferred_holding(module) {
        return MovePlan::Refused {
            why: feedback::NOT_HELD_TEXT,
        };
    }
    if layout.is_away(module) {
        return MovePlan::Ask {
            module: module.clone(),
            peer,
        };
    }
    drop(layout);
    if w.peers()
        .any(|(_, p)| p.moving(module) || p.pages_waiting.contains_key(module))
    {
        return MovePlan::Refused {
            why: feedback::MOVING_TEXT,
        };
    }
    match w.lifecycle.kind(module).and_then(MoveKind::of) {
        Some(kind) => MovePlan::Peer {
            module: module.clone(),
            peer,
            kind,
        },
        None => MovePlan::Refused {
            why: feedback::NOT_HELD_TEXT,
        },
    }
}

/// Where one move stands, as far as this window's own work for it goes; `Moves` says where it stands on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Source: offered, the module untouched.
    Offered,
    /// Source: frozen, its record and handoff asked of the host.
    Freezing,
    /// Source: `move` went out; only `move_done`, `move_abort` or the link's end decides it now.
    Sent,
    /// Destination: accepted, its place being prepared.
    Accepted,
    /// Destination: its place is ready and `move_ready` went out.
    Prepared,
    /// Destination, a chat: `move` arrived, and `move_done` waits until this window's presence holds the arriving
    /// session's running turn, so neither window can revert a file the turn may be writing.
    Holding,
    /// Destination: `move` arrived and `move_done` is queued.
    Received,
}

#[derive(Debug)]
struct Entry {
    /// The module here: on the source the moving module, on the destination the id it arrives as.
    module: ModuleId,
    /// The id the source gave the module, which its focus intent names.
    offered: ModuleId,
    kind: MoveKind,
    role: Role,
    /// Source: the keys go with the module once it is committed.
    follow: bool,
    stage: Stage,
    /// Destination: where the module sat in the source.
    shape: Shape,
    /// Destination: the kind's record that `move` carried.
    record: serde_json::Value,
    /// Destination: the descriptors `move` brought, put back here once the commit hands them over.
    fds: Option<Descriptors>,
    /// Source: when the deadline next speaks for this move.
    deadline: Option<Instant>,
    /// Source: the move was sent and the other window has said nothing since its deadline: the module stays frozen,
    /// and a kill of it asks whether to end that window instead.
    silent: bool,
    /// A chat: the tab that moves (on the source; the destination learns it only from the record).
    tab: Option<TabId>,
    /// A chat: its agent's session id, as the offer named it.
    session: Option<String>,
    /// A chat: this window asked the editor's review events of `session` to be held, and owes their release.
    review_held: bool,
    /// Destination, a chat: this window's presence was asked to hold the arriving session, and lets it go should
    /// the arrival be given up.
    presence_asked: bool,
}

impl Entry {
    /// The chat view a chat's entry moves out of or into: the base view for `agent`, else its numbered view.
    fn view(&self) -> ViewId {
        chat::view_of(&self.module)
    }
}

/// What an unanswered compositor raise will send once the other window is ready.
#[derive(Debug, Clone)]
pub enum Raise {
    Focus(FocusTarget),
    Workspace(String),
}

impl From<FocusTarget> for Raise {
    fn from(target: FocusTarget) -> Self {
        Self::Focus(target)
    }
}

struct WaitingPage {
    timer: TimerToken,
    /// When the commit started the wait.
    since: std::time::Instant,
    workspace: String,
    /// A workspace arrival is shown by its workspace, never by the document becoming ready.
    show: bool,
}

/// A window's link to the other window of the pair and the moves on it; present while a peer is attached.
pub struct PeerMoves {
    peer: PeerId,
    name: WindowName,
    port: Rc<dyn PeerPort>,
    moves: Moves,
    side: Side,
    entries: BTreeMap<MoveId, Entry>,
    /// The destination's committed arrivals: the id the source gave each, and the id it took here.
    arrived: BTreeMap<ModuleId, ModuleId>,
    /// The lifecycle entry each committed arrival replaced here (by the id it took), put back should the host fail to
    /// build it.
    replaced: BTreeMap<ModuleId, SavedEntry>,
    /// What each `RaisePeer` still unanswered is for, oldest first: the focus to send once the other window is up.
    raises: VecDeque<Raise>,
    pub(super) held: Vec<ModuleId>,
    pub(super) monitor: Option<String>,
    /// Chats whose move waits for their turn's end, since their agent cannot keep a running turn across a move.
    queued: Vec<chat::Queued>,
    /// The deadline's timer while one is armed, and when it fires.
    deadline: Option<(TimerToken, Instant)>,
    /// The other window's last summary.
    summary: Option<Summary>,
    debounce: crate::window_link::summary::SummaryDebounce,
    summary_timer: Option<TimerToken>,
    own_summary: Summary,
    summary_inputs: (u64, u64),
    counts_dirty: bool,
    pages_waiting: BTreeMap<ModuleId, WaitingPage>,
    after_batch: bool,
    groups: workspace::Groups,
}

impl PeerMoves {
    /// The moves of a window whose role makes it `side` of the pair, talking through `port`.
    pub fn new(peer: PeerId, name: WindowName, port: Rc<dyn PeerPort>, side: Side) -> PeerMoves {
        PeerMoves {
            peer,
            name,
            port,
            moves: Moves::new(side),
            side,
            entries: BTreeMap::new(),
            arrived: BTreeMap::new(),
            replaced: BTreeMap::new(),
            raises: VecDeque::new(),
            held: Vec::new(),
            monitor: None,
            queued: Vec::new(),
            deadline: None,
            summary: None,
            debounce: Default::default(),
            summary_timer: None,
            own_summary: Summary::default(),
            summary_inputs: (0, 0),
            counts_dirty: false,
            pages_waiting: BTreeMap::new(),
            after_batch: false,
            groups: workspace::Groups::default(),
        }
    }

    pub fn peer(&self) -> PeerId {
        self.peer
    }

    /// The name the peer's window answers to.
    pub fn name(&self) -> &WindowName {
        &self.name
    }

    /// The other window's counts and workspace names, as it last sent them.
    pub fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }

    /// Whether `module` has a move in flight from this window.
    pub fn moving(&self, module: &ModuleId) -> bool {
        self.entries
            .values()
            .any(|e| e.role == Role::Source && e.module == *module)
    }

    /// The ids modules arriving here took and that are not placed yet: no new instance may take them.
    pub(crate) fn arriving(&self) -> Vec<ModuleId> {
        self.entries
            .values()
            .filter(|e| e.role == Role::Destination)
            .map(|e| e.module.clone())
            .collect()
    }

    fn entry_of(&self, role: Role, module: &ModuleId, stages: &[Stage]) -> Option<MoveId> {
        self.entries
            .iter()
            .find(|(_, e)| e.role == role && e.module == *module && stages.contains(&e.stage))
            .map(|(id, _)| *id)
    }

    /// The text of the note a sent move gets when the other window stays silent.
    fn not_answering(&self) -> String {
        feedback::not_answering_text(other_number(self.side))
    }
}

/// The number of the other window of the pair, seen from `side`.
fn other_number(side: Side) -> u8 {
    match side {
        Side::One => 2,
        Side::Two => 1,
    }
}

/// The move a closing window hands `module` over with: one already in flight to any peer, else a new offer to
/// window 2. The move is named by its peer and id.
pub(super) fn offer_for_close(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    module: &ModuleId,
    selected_tab: Option<TabId>,
    selected: Option<Result<ChatMoveFacts, String>>,
    batch: &mut Batch,
) -> Option<(PeerId, MoveId)> {
    let tab = selected_tab.or_else(|| selected.as_ref().and_then(|f| f.as_ref().ok()).map(|f| f.tab));
    let in_flight = w.peers().find_map(|(peer, p)| {
        p.entries
            .iter()
            .find(|(_, e)| e.role == Role::Source && e.module == *module && (tab.is_none() || e.tab == tab))
            .map(|(id, _)| (*peer, *id))
    });
    if in_flight.is_some() {
        return in_flight;
    }
    let peer = w.second_peer()?;
    // A refusal is toasted where it happens and leaves nothing for the close to wait for.
    offer_selected(w, cx, win, peer, module, None, false, selected, batch)
        .ok()
        .flatten()
        .map(|id| (peer, id))
}

/// Every move this window sends that has not ended, named by its peer and id.
pub(super) fn active_source_moves(w: &Window) -> Vec<(PeerId, MoveId)> {
    w.peers()
        .flat_map(|(peer, p)| {
            p.entries
                .iter()
                .filter(|(_, e)| e.role == Role::Source)
                .map(move |(id, _)| (*peer, *id))
        })
        .collect()
}

/// The window is torn down while moves it sent are undecided: each such module is let go here, never shut down. The
/// other window may already have written the `move_done` this window will never read, and then the shell or session
/// runs on there; if it has not, it closes what `move` carried once the link's end reaches it, and the shell or
/// session ends with its last descriptor as it would have with the shutdown. Moves not yet sent are left to the
/// teardown's shutdown: nothing of them has left this window.
pub(super) fn forget_sent_at_teardown(w: &mut Window, win: WinId, batch: &mut Batch) {
    let mut forgotten = Vec::new();
    for p in w.peers.values_mut() {
        let sent: Vec<MoveId> = p
            .entries
            .iter()
            .filter(|(_, e)| e.role == Role::Source && e.stage == Stage::Sent)
            .map(|(id, _)| *id)
            .collect();
        for id in sent {
            if let Some(entry) = p.entries.remove(&id) {
                batch.push_effect(forget_op(&entry, win));
                forgotten.push(entry.module);
            }
        }
    }
    for module in forgotten {
        w.say(format!(
            "[move] {module}: sent and not yet answered as the window closes; let go, not ended"
        ));
    }
}

pub(super) fn note_turn_wait(w: &mut Window, module: &ModuleId) {
    let number = if other_name(w) == "2" { 2 } else { 1 };
    let text = feedback::moves_at_turn_end_text(number);
    w.say(format!("[move] {module}: its turn runs; {text}"));
    chrome::show_toast(w, text, ToastKind::Other);
}

/// Whether move `id` to `peer` has a verdict, or its peer is gone.
pub(super) fn move_decided(w: &Window, peer: PeerId, id: MoveId) -> bool {
    w.peer(peer).is_none_or(|p| p.groups.outcomes.contains_key(&id))
}

/// The other window, when `module` of `w` is frozen for a move it was sent and that window has not answered since
/// its deadline, with that window's number: then the user may end that window, and the link's end settles the move.
pub(crate) fn silent_peer(w: &Window, module: &ModuleId) -> Option<(PeerId, u8)> {
    w.peers().find_map(|(_, p)| {
        p.entries
            .values()
            .any(|e| e.role == Role::Source && e.module == *module && e.silent)
            .then(|| (p.peer, other_number(p.side)))
    })
}

/// What `peer`'s last summary counted running and queued: what ends with it.
pub(crate) fn peer_counts(w: &Window, peer: PeerId) -> (u32, u32) {
    w.peer(peer)
        .and_then(PeerMoves::summary)
        .map(|s| (s.running, s.queued))
        .unwrap_or_default()
}

/// The side of the pair a window of `role` is: the second end of a link is window 2, everything else window 1.
pub fn side_of(role: &WindowRole) -> Side {
    match role {
        WindowRole::Linked { side: LinkSide::Second } => Side::Two,
        WindowRole::Main | WindowRole::Companion | WindowRole::Linked { side: LinkSide::First } => Side::One,
    }
}

/// Whether `module` of `w` has a move in flight from this window to any peer: until the move ends, nothing here may
/// act on the session the other window may already be taking over.
pub(crate) fn moving(w: &Window, module: &ModuleId) -> bool {
    w.peers().any(|(_, p)| p.moving(module))
}

/// The ids modules arriving in `w` from any peer took and that are not placed yet; a new instance here must not take
/// one, or the arrival would land on a module that already holds its name.
pub(crate) fn arriving(w: &Window) -> Vec<ModuleId> {
    w.peers().flat_map(|(_, p)| p.arriving()).collect()
}

/// Runs `f` on `peer`, taken out for the call so both it and the window can be changed; `None` when it is not
/// attached.
fn with_peer<R>(w: &mut Window, peer: PeerId, f: impl FnOnce(&mut Window, &mut PeerMoves) -> R) -> Option<R> {
    let mut p = w.peers.remove(&peer)?;
    let out = f(w, &mut p);
    w.peers.insert(peer, p);
    Some(out)
}

/// The peer a pair verb addresses: window 2. With none attached the verb is refused in the words of the role.
pub(super) fn pair_peer(w: &mut Window, win: WinId, batch: &mut Batch) -> Option<PeerId> {
    let peer = w.second_peer();
    if peer.is_none() {
        refuse(w, win, feedback::no_other_window_text(w.role()), batch);
    }
    peer
}

/// The peer that has an entry for move `mv` that `want` accepts: a host answer names its move by id alone, and move
/// ids are numbered per link, so the entry decides which link it is for.
fn peer_with(w: &Window, mv: MoveId, want: impl Fn(&Entry) -> bool) -> Option<PeerId> {
    w.peers()
        .find(|(_, p)| p.entries.get(&mv).is_some_and(&want))
        .map(|(peer, _)| *peer)
}

/// A refusal of the move verb, said and toasted.
fn refuse(w: &mut Window, win: WinId, text: &str, batch: &mut Batch) {
    w.say(format!("[move] refused: {text}"));
    w.refuse(win, text.to_owned(), batch);
}

fn send(w: &mut Window, p: &PeerMoves, msg: Msg) {
    let kind = msg.kind();
    if let Err(err) = p.port.send(msg, None) {
        w.warn(format!("[move] {kind} could not be sent: {err}"));
    }
}

/// Where `module` sits now, as the other window lays it out: a float by its rectangle relative to the grid.
fn shape_of(w: &Window, cx: &Cx, win: WinId, module: &ModuleId) -> Shape {
    let Ok(layout) = w.layout.try_borrow() else {
        return Shape::Tiled;
    };
    let Some(float) = layout.float_of(module) else {
        return Shape::Tiled;
    };
    let grid = cx.reads.grid_size(win);
    if grid.w <= 0 || grid.h <= 0 {
        return Shape::Tiled;
    }
    let (fw, fh) = (float.rect.w as f64 / grid.w as f64, float.rect.h as f64 / grid.h as f64);
    Shape::Float {
        x: float.rect.cx as f64 - fw / 2.0,
        y: float.rect.cy as f64 - fh / 2.0,
        w: fw,
        h: fh,
    }
}

// The source's side.

/// `move.to-window` for `module`: planned, and for the other window offered. Nothing is frozen yet; the deadline
/// starts.
pub fn offer(w: &mut Window, cx: &Cx, win: WinId, peer: PeerId, module: &ModuleId, follow: bool, batch: &mut Batch) {
    offer_tab(w, cx, win, peer, module, None, follow, batch);
}

/// [`offer`], for a chat the tab holding `session` when one is named (the base view holds several), else the tab
/// its view moves. A chat whose agent cannot keep a running turn across a move, and whose turn runs, waits for the
/// turn's end instead.
pub(super) fn offer_tab(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    peer: PeerId,
    module: &ModuleId,
    session: Option<&str>,
    follow: bool,
    batch: &mut Batch,
) {
    let _ = offer_selected(w, cx, win, peer, module, session, follow, None, batch);
}

/// The move `module` was offered as; `Ok(None)` when it waits for a turn's end or the other window's own offer, and
/// the refusal's reason, already said and toasted, otherwise.
fn offer_selected(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    peer: PeerId,
    module: &ModuleId,
    session: Option<&str>,
    follow: bool,
    selected: Option<Result<ChatMoveFacts, String>>,
    batch: &mut Batch,
) -> Result<Option<MoveId>, String> {
    if w.peer(peer).is_none() {
        let text = feedback::no_other_window_text(w.role());
        refuse(w, win, text, batch);
        return Err(text.into());
    };
    let moving_base = *module == ModuleId::agent() && selected.is_some();
    let planned = if moving_base && w.lifecycle.kind(module).is_some() {
        MovePlan::Peer {
            module: module.clone(),
            peer,
            kind: MoveKind::Chat,
        }
    } else {
        plan(w, module, &MoveTarget::Peer(peer))
    };
    let kind = match planned {
        MovePlan::Refused { why } => {
            refuse(w, win, why, batch);
            return Err(why.to_owned());
        }
        MovePlan::Ask { module, .. } => {
            ask(w, peer, &module, follow);
            return Ok(None);
        }
        MovePlan::Peer { kind, .. } => kind,
    };
    let chat = match kind {
        MoveKind::Chat => match selected.unwrap_or_else(|| cx.reads.chat_move(win, chat::view_of(module), session)) {
            Ok(facts) => Some(facts),
            Err(why) => {
                refuse(w, win, &why, batch);
                return Err(why);
            }
        },
        _ => None,
    };
    if chat.as_ref().is_some_and(|facts| {
        w.peers().any(|(_, p)| {
            p.entries
                .values()
                .any(|e| e.role == Role::Source && e.kind == MoveKind::Chat && e.tab == Some(facts.tab))
        })
    }) {
        refuse(w, win, feedback::MOVING_TEXT, batch);
        return Err(feedback::MOVING_TEXT.into());
    }
    if let Some(facts) = chat.as_ref().filter(|facts| facts.running && !facts.live) {
        chat::queue(w, peer, module, facts, follow);
        return Ok(None);
    }
    let shape = shape_of(w, cx, win, module);
    // Zero when the host cannot tell: the other window then lays the module out at its own size.
    let mut size = cx
        .reads
        .module_size(win, module)
        .map(|size| (size.w.max(0) as u32, size.h.max(0) as u32))
        .unwrap_or((0, 0));
    if kind == MoveKind::PluginPage {
        size = if cx.reads.page_built(win, module) {
            (size.0.max(1), size.1.max(1))
        } else {
            (0, 0)
        };
    }
    with_peer(w, peer, |w, p| {
        let now = w.now();
        let id = p.moves.offer(now);
        let msg = Msg::MoveOffer {
            mv: id.0,
            module: module.to_string(),
            kind: kind.wire().to_owned(),
            shape,
            size,
            session: chat.as_ref().and_then(|facts| facts.session.clone()),
        };
        if let Err(err) = p.port.send(msg, None) {
            // Nothing was frozen: the module never left.
            p.moves.aborted(id, &err);
            refuse(w, win, &err, batch);
            return Err(err.to_string());
        }
        let mut entry = Entry {
            module: module.clone(),
            offered: module.clone(),
            kind,
            role: Role::Source,
            follow,
            stage: Stage::Offered,
            shape,
            record: if kind == MoveKind::PluginPage {
                serde_json::json!({"built": size != (0, 0)})
            } else {
                serde_json::Value::Null
            },
            fds: None,
            deadline: Some(now + MOVE_DEADLINE),
            silent: false,
            tab: chat.as_ref().map(|facts| facts.tab),
            session: chat.and_then(|facts| facts.session),
            review_held: false,
            presence_asked: false,
        };
        chat::hold_review(w, win, &mut entry, batch);
        p.entries.insert(id, entry);
        w.say(format!("[move] {module}: offered to the other window as move {}", id.0));
        rearm(w, p);
        Ok(Some(id))
    })
    .unwrap_or(Ok(None))
}

/// The other window holds `module`, which this window shows as away: that window is asked to offer it here. What
/// it answers is its own move's: an offer, or a refusal it shows itself.
fn ask(w: &mut Window, peer: PeerId, module: &ModuleId, follow: bool) {
    with_peer(w, peer, |w, p| {
        let msg = Msg::MoveAsk {
            module: Some(module.to_string()),
            session: None,
            follow,
        };
        match p.port.send(msg, None) {
            Ok(()) => w.say(format!(
                "[move] {module}: the other window holds it; asked it to move it here"
            )),
            Err(err) => w.warn(format!("[move] {module}: move_ask could not be sent: {err}")),
        }
    });
}

/// The other window asks for a module this window holds: offered to it as this window's own `move.to-window` offers
/// one, its refusals shown here. A chat may be asked for by its session instead, which names the view holding it
/// and, in the base view, the tab. A module or a session this window does not hold is said and dropped, as a key
/// for a missing module is. The other window names a module by the id it had there, so one that arrived here under
/// another id is found by the id it took; one this window shows as away is not here to offer, and is not asked back
/// for, since that would only send the asking window's own copy on.
fn asked(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    peer: PeerId,
    module: Option<ModuleId>,
    session: Option<String>,
    follow: bool,
    batch: &mut Batch,
) {
    let (module, session) = match (module, session) {
        (Some(named), _) => {
            let module = w
                .peer(peer)
                .and_then(|p| p.arrived.get(&named).cloned())
                .unwrap_or(named);
            let away = w
                .layout
                .try_borrow()
                .is_ok_and(|l| l.contains(&module) && l.is_away(&module));
            if away {
                w.warn(format!(
                    "[move] the other window asked for {module}, which has left this window too"
                ));
                return;
            }
            (module, None)
        }
        (None, Some(session)) => match cx.reads.view_of_session(win, &session) {
            Some(view) => (view.module(), Some(session)),
            None => {
                w.warn(format!(
                    "[move] the other window asked for session {session:?}, which no chat here holds"
                ));
                return;
            }
        },
        (None, None) => {
            w.warn("[move] the other window asked for a move naming neither a module nor a session".into());
            return;
        }
    };
    w.say(format!("[move] {module}: the other window asks for it"));
    offer_tab(w, cx, win, peer, &module, session.as_deref(), follow, batch);
}

/// The destination is ready: the module is frozen and its record asked of the host. An answer out of turn
/// changes nothing.
fn ready(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    if !p.moves.ready(id) {
        return;
    }
    let now = w.now();
    if !p.moves.freeze(id, now) {
        return;
    }
    let Some(entry) = p.entries.get_mut(&id) else {
        return;
    };
    entry.stage = Stage::Freezing;
    entry.deadline = Some(now + MOVE_DEADLINE);
    // The module stays shown where it is; a frozen session reads and writes nothing.
    if entry.kind == MoveKind::LuaPanel || (entry.kind == MoveKind::PluginPage && entry.record["built"] == false) {
        let record = if entry.kind == MoveKind::LuaPanel {
            serde_json::Value::Null
        } else {
            serde_json::json!({"built": false})
        };
        if let Err(why) = p.port.send(Msg::Move { mv: id.0, record }, None) {
            give_up(w, p, win, id, why, batch);
        } else if p.moves.sent(id, now) {
            p.entries.get_mut(&id).unwrap().stage = Stage::Sent;
        }
    } else {
        batch.push_effect(freeze_op(entry, win, id));
    }
    rearm(w, p);
}

/// The kind's freeze of `entry`'s module for move `mv`, answered with the move's id.
fn freeze_op(entry: &Entry, win: WinId, mv: MoveId) -> HostOp {
    match entry.kind {
        MoveKind::Chat => HostOp::Chat(ChatOp::Freeze {
            win,
            view: entry.view(),
            tab: entry.tab,
            mv,
        }),
        MoveKind::PluginPage | MoveKind::LuaPanel => HostOp::Plugin(PluginOp::Freeze {
            win,
            module: entry.module.clone(),
            mv,
        }),
        MoveKind::Terminal => HostOp::Terminal(TermOp::Detach {
            win,
            module: entry.module.clone(),
            mv,
        }),
    }
}

/// The kind's resume of `entry`'s frozen module: the move did not happen.
fn resume_op(entry: &Entry, win: WinId) -> HostOp {
    resume_of(entry.kind, win, &entry.module, entry.tab)
}

/// The resume of a frozen `module` of `kind`: for a chat, of its tab `tab`, or of the tab its view has frozen when
/// the move that froze it has ended and its tab is no longer known.
fn resume_of(kind: MoveKind, win: WinId, module: &ModuleId, tab: Option<TabId>) -> HostOp {
    match kind {
        MoveKind::Chat => HostOp::Chat(ChatOp::Resume {
            win,
            view: chat::view_of(module),
            tab,
        }),
        MoveKind::PluginPage | MoveKind::LuaPanel => HostOp::Plugin(PluginOp::Resume {
            win,
            module: module.clone(),
        }),
        MoveKind::Terminal => HostOp::Terminal(TermOp::Resume {
            win,
            module: module.clone(),
        }),
    }
}

/// The kind's letting go of `entry`'s module, which the other window holds now.
fn forget_op(entry: &Entry, win: WinId) -> HostOp {
    match entry.kind {
        MoveKind::Chat => HostOp::Chat(ChatOp::Forget {
            win,
            view: entry.view(),
            tab: entry.tab,
        }),
        MoveKind::PluginPage | MoveKind::LuaPanel => HostOp::Plugin(PluginOp::Forget {
            win,
            module: entry.module.clone(),
        }),
        MoveKind::Terminal => HostOp::Terminal(TermOp::Forget {
            win,
            module: entry.module.clone(),
        }),
    }
}

/// The host froze `module` (a terminal's `Detached`, a chat's `Frozen`) for move `mv` and gave its record and
/// handoff: `move` goes out with the handoff, unread here. The answer is matched to its move by `mv`, never by the
/// module alone: a move that expired while frozen and a retry of the same module may both be waiting for a freeze,
/// and the first one's late answer carries a snapshot taken before the module read again.
#[allow(clippy::too_many_arguments)]
pub fn frozen(
    w: &mut Window,
    win: WinId,
    kind: MoveKind,
    module: &ModuleId,
    mv: MoveId,
    record: serde_json::Value,
    handoff: Handoff,
    batch: &mut Batch,
) {
    let peer = peer_with(w, mv, |e| {
        e.role == Role::Source && e.module == *module && e.kind == kind
    });
    let stage = peer.and_then(|peer| w.peer(peer)?.entries.get(&mv)).map(|e| e.stage);
    match stage {
        Some(Stage::Freezing) => {}
        Some(Stage::Sent) => {
            // A repeated freeze answer: the other window may already hold the descriptors, so this window must not
            // read them again; only that move's end decides.
            w.warn(format!(
                "[move] {module}: a second freeze answer for move {}, which is sent; ignored",
                mv.0
            ));
            return;
        }
        _ => {
            let frozen_for = w
                .peers()
                .find_map(|(_, p)| p.entry_of(Role::Source, module, &[Stage::Freezing, Stage::Sent]));
            if let Some(current) = frozen_for {
                // Another move of the module has it frozen or sent: reading it again here would let both windows
                // run one session. The stale answer's handoff is dropped unread.
                w.warn(format!(
                    "[move] {module}: a freeze answer for move {}, which has ended, while move {} holds it; ignored",
                    mv.0, current.0
                ));
                return;
            }
            // A freeze no move waits for any more: the module reads again, and the handoff is dropped unread.
            w.warn(format!(
                "[move] {module}: froze for move {}, which has ended; resumed",
                mv.0
            ));
            batch.push_effect(resume_of(kind, win, module, None));
            return;
        }
    }
    let id = mv;
    let mut record = record;
    let Some(peer) = peer else { return };
    with_peer(w, peer, |w, p| {
        if let Some(meta) = p.entries.get(&id).and_then(|e| e.record.get("workspace_tab")).cloned() {
            if let Some(object) = record.as_object_mut() {
                object.insert("workspace_tab".into(), meta);
                object.insert("workspace_move_id".into(), id.0.into());
            }
        }
        match p.port.send(Msg::Move { mv: id.0, record }, Some(handoff)) {
            Ok(()) => {
                let now = w.now();
                if p.moves.sent(id, now) {
                    if let Some(entry) = p.entries.get_mut(&id) {
                        entry.stage = Stage::Sent;
                        entry.deadline = Some(now + MOVE_DEADLINE);
                    }
                    w.say(format!("[move] {module}: sent as move {}", id.0));
                }
            }
            Err(err) => give_up(w, p, win, id, err, batch),
        }
        rearm(w, p);
    });
}

/// Which freeze could not be made: a terminal's, whose refusal names its module, or a chat's, whose refusal names
/// its move.
#[derive(Debug, Clone)]
pub enum FreezeOf {
    Module(ModuleId),
    Move { module: ModuleId, mv: MoveId },
}

/// The host could not freeze a module: its move is given up before anything left. A refusal naming its move gives
/// up that move alone, and only while it waits for that freeze: a late refusal of a move that has ended never gives
/// up the move that followed it.
pub fn freeze_refused(w: &mut Window, win: WinId, of: FreezeOf, why: String, batch: &mut Batch) {
    let found = match &of {
        FreezeOf::Module(module) => w.peers().find_map(|(peer, p)| {
            p.entry_of(Role::Source, module, &[Stage::Freezing])
                .map(|id| (*peer, id))
        }),
        FreezeOf::Move { module, mv } => peer_with(w, *mv, |e| {
            e.role == Role::Source && e.module == *module && e.stage == Stage::Freezing
        })
        .map(|peer| (peer, *mv)),
    };
    if let Some((peer, id)) = found {
        with_peer(w, peer, |w, p| {
            give_up(w, p, win, id, why, batch);
            rearm(w, p);
        });
    }
}

/// Whether `module` of `w` is a chat frozen for a move this window sends: a refused chat call naming it is that
/// freeze's refusal.
pub(crate) fn chat_freezing(w: &Window, module: &ModuleId) -> Option<MoveId> {
    w.peers().find_map(|(_, p)| {
        p.entry_of(Role::Source, module, &[Stage::Freezing])
            .filter(|id| p.entries.get(id).is_some_and(|e| e.kind == MoveKind::Chat))
    })
}

/// The source gives a move up by itself, only ever before `move` left: the destination is told so it drops what it
/// prepared, and a frozen terminal reads again.
fn give_up(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, why: String, batch: &mut Batch) {
    p.moves.aborted(id, &why);
    send(
        w,
        p,
        Msg::MoveAbort {
            mv: id.0,
            why: why.clone(),
        },
    );
    if let Some(entry) = p.entries.remove(&id) {
        workspace::decided(
            p,
            id,
            &entry,
            crate::window_link::workspace_move::Verdict::Refused(why.clone()),
        );
        if entry.stage != Stage::Offered {
            batch.push_effect(resume_op(&entry, win));
        }
        chat::release_review(win, &entry, true, batch);
    }
    refuse(w, win, &why, batch);
}

/// What `Moves` said of the source's moves, carried out.
fn verdicts(w: &mut Window, p: &mut PeerMoves, win: WinId, verdicts: Vec<(MoveId, SourceVerdict)>, batch: &mut Batch) {
    for (id, verdict) in verdicts {
        verdict_for(w, p, win, id, verdict, batch);
    }
}

fn verdict_for(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, verdict: SourceVerdict, batch: &mut Batch) {
    match verdict {
        SourceVerdict::Committed => committed(w, p, win, id, batch),
        SourceVerdict::Resume(why) => {
            let Some(entry) = p.entries.remove(&id) else {
                return;
            };
            workspace::decided(
                p,
                id,
                &entry,
                crate::window_link::workspace_move::Verdict::Refused(why.clone()),
            );
            // Only offered, nothing was frozen and nothing is to resume.
            if entry.stage != Stage::Offered {
                batch.push_effect(resume_op(&entry, win));
            }
            chat::release_review(win, &entry, true, batch);
            w.say(format!("[move] {}: stays here: {why}", entry.module));
            w.refuse(win, why, batch);
        }
        SourceVerdict::NotAnswering => {
            let text = p.not_answering();
            let Some(entry) = p.entries.get_mut(&id) else {
                return;
            };
            // Said once; the module stays frozen, since only the other window or the link's end decides a sent move.
            entry.deadline = None;
            entry.silent = true;
            let module = entry.module.clone();
            w.warn(format!("[move] {module}: {text}"));
            chrome::show_toast(w, text, ToastKind::Other);
        }
    }
}

/// The destination holds the module: the local copy goes, its leaf stays as an away placeholder in every workspace,
/// and the keys, if it held them, go first to the module the layout gives them.
fn committed(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    let Some(entry) = p.entries.remove(&id) else {
        return;
    };
    workspace::decided(p, id, &entry, crate::window_link::workspace_move::Verdict::Committed);
    batch.push_effect(forget_op(&entry, win));
    chat::release_review(win, &entry, false, batch);
    if chat::is_base_tab(&entry) {
        // The base view stays here with its other tabs: only the tab left, so nothing is marked away and the keys
        // stay where they are.
        w.say(format!("[move] {}: its tab is in the other window now", entry.module));
        if entry.follow {
            queue_raise(
                p,
                win,
                FocusTarget::Module {
                    id: entry.module.to_string(),
                },
                batch,
            );
        }
        return;
    }
    let module = entry.module;
    let layout = w.layout.clone();
    let mut unplaced = false;
    let next = match layout.try_borrow_mut() {
        Ok(mut l) => {
            if entry.kind == MoveKind::PluginPage && !l.contains(&module) {
                // A deferred holding gets its first leaf only to retain the away identity; no lifecycle sees it shown.
                unplaced = true;
                let focus = l.focus().clone();
                if crate::layout::place(&mut l, &module, &focus, crate::layout::Axis::Column).is_err() {
                    let _ = l.add_float(
                        &module,
                        FloatRect::centred(crate::layout::rules::NOMINAL_GRID, DEFAULT_FLOAT_SHARE),
                        false,
                    );
                }
                let _ = l.set_focus(&focus);
            }
            let had_keys = *l.focus() == module;
            if let Err(err) = l.set_away(&module, true) {
                w.warn(format!("[move] BUG: {module} could not be marked away: {err}"));
            }
            had_keys.then(|| l.focus().clone()).filter(|next| *next != module)
        }
        Err(_) => {
            w.warn(format!("[move] BUG: the layout was borrowed when {module} left"));
            None
        }
    };
    let workspaces = w.workspaces.clone();
    match workspaces.try_borrow_mut() {
        Ok(mut workspaces) => {
            if unplaced {
                workspaces.add_everywhere(&module);
            }
            workspaces.set_away_everywhere(&module, true);
        }
        Err(_) => w.warn(format!("[move] BUG: the workspaces were borrowed when {module} left")),
    }
    let focus = next.map(|next| {
        let tail = arrive_tail(win, &next, false);
        (next, tail)
    });
    w.changed_with_focus(win, focus, batch);
    w.say(format!("[move] {module}: in the other window now"));
    if entry.follow {
        queue_raise(p, win, FocusTarget::Module { id: module.to_string() }, batch);
    }
}

// The destination's side.

/// The other window offers a module: accepted and laid out without revealing it at its size and shape, or refused. A
/// tab of the other window's base view comes into this window's base view, which is always here: it needs no place,
/// so it is ready at once.
#[allow(clippy::too_many_arguments)]
fn offered(
    w: &mut Window,
    p: &mut PeerMoves,
    win: WinId,
    id: MoveId,
    module: ModuleId,
    kind: MoveKind,
    shape: Shape,
    size: (u32, u32),
    session: Option<String>,
    batch: &mut Batch,
) {
    if !p.moves.accept_offer(id) {
        send(
            w,
            p,
            Msg::MoveRefused {
                mv: id.0,
                why: feedback::NOT_AN_OFFER_TEXT.to_owned(),
            },
        );
        return;
    }
    if kind == MoveKind::Chat && module == ModuleId::agent() {
        chat::base_tab_offered(w, p, win, id, session, batch);
        return;
    }
    let (layout, workspaces) = (w.layout.clone(), w.workspaces.clone());
    let (here_live, other_held, held_here) = match (layout.try_borrow(), workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => {
            // Every name a new instance here could not take is one an arrival cannot take either: what any
            // workspace holds, what a later build's kept data names, what is being made and what is arriving,
            // and the other window's latest held set.
            let mut live = BTreeSet::new();
            let mut away = BTreeSet::new();
            for (_, layout) in workspaces.each(&layout) {
                let held_away = layout.away();
                for module in layout.modules() {
                    if held_away.contains(&module) {
                        away.insert(module);
                    } else {
                        live.insert(module);
                    }
                }
            }
            // `p` is out of the window's map here: what the other peers hold or have arriving is read from the
            // window and added to this peer's own, so no two arrivals of any links take one name.
            let mut arriving = arriving(w);
            arriving.extend(p.arriving());
            // A module of one name only (a page, a Lua panel) the other window can send only while this window holds
            // it away: one placed here and not away, or arriving here already, is this window's, and an offer of it
            // would take over the page this window shows.
            let held_here = !module.kind().has_instances() && (live.contains(&module) || arriving.contains(&module));
            live.extend(workspaces.reserved());
            live.extend(w.modules.pending_ids());
            live.extend(arriving);
            away.extend(w.held_union());
            away.extend(p.held.iter().cloned());
            (
                live.into_iter().collect::<Vec<_>>(),
                away.into_iter().collect::<Vec<_>>(),
                held_here,
            )
        }
        _ => {
            p.moves.source_aborted(id);
            send(
                w,
                p,
                Msg::MoveRefused {
                    mv: id.0,
                    why: feedback::LAYOUT_CHANGING_TEXT.to_owned(),
                },
            );
            return;
        }
    };
    if held_here {
        p.moves.source_aborted(id);
        workspace::refused_offer(p, id, &module, feedback::HELD_HERE_TEXT.to_owned());
        w.warn(format!(
            "[move] {module}: offered by the other window while this window holds it; refused"
        ));
        send(
            w,
            p,
            Msg::MoveRefused {
                mv: id.0,
                why: feedback::HELD_HERE_TEXT.to_owned(),
            },
        );
        return;
    }
    let arrival = match arrival_id(&module, &here_live, &other_held) {
        Ok(arrival) => arrival,
        Err(err) => {
            // Taken back out: an accepted offer nothing was prepared for.
            p.moves.source_aborted(id);
            workspace::refused_offer(p, id, &module, err.to_string());
            send(
                w,
                p,
                Msg::MoveRefused {
                    mv: id.0,
                    why: err.to_string(),
                },
            );
            return;
        }
    };
    w.say(format!("[move] {module}: arriving as {arrival} (move {})", id.0));
    let mut entry = Entry {
        module: arrival.clone(),
        offered: module,
        kind,
        role: Role::Destination,
        follow: false,
        stage: Stage::Accepted,
        shape,
        record: serde_json::Value::Null,
        fds: None,
        deadline: None,
        silent: false,
        tab: None,
        session,
        review_held: false,
        presence_asked: false,
    };
    chat::hold_review(w, win, &mut entry, batch);
    p.entries.insert(id, entry);
    // Made before it is mapped: the module is laid out at its final size before the source freezes anything.
    if kind == MoveKind::LuaPanel || (kind == MoveKind::PluginPage && size == (0, 0)) {
        if p.moves.prepared(id) {
            p.entries.get_mut(&id).unwrap().stage = Stage::Prepared;
            send(w, p, Msg::MoveReady { mv: id.0 });
        }
        return;
    }
    batch.push_layout(HostOp::PrepareSurface {
        win,
        module: arrival,
        shape,
        size,
    });
}

/// The peer whose offer of `module` this window accepted and whose place the host has not yet made.
fn accepting_peer(w: &Window, module: &ModuleId) -> Option<PeerId> {
    w.peers()
        .find(|(_, p)| p.entry_of(Role::Destination, module, &[Stage::Accepted]).is_some())
        .map(|(peer, _)| *peer)
}

/// The host has the arriving module's place ready: the source may freeze it.
pub fn prepared(w: &mut Window, module: &ModuleId) {
    let Some(peer) = accepting_peer(w, module) else { return };
    with_peer(w, peer, |w, p| {
        let Some(id) = p.entry_of(Role::Destination, module, &[Stage::Accepted]) else {
            return;
        };
        if !p.moves.prepared(id) {
            return;
        }
        if let Some(entry) = p.entries.get_mut(&id) {
            entry.stage = Stage::Prepared;
        }
        send(w, p, Msg::MoveReady { mv: id.0 });
    });
}

/// The host could not prepare the arriving module's place: the offer is refused, and the source, which froze
/// nothing yet, keeps it.
pub fn prepare_refused(w: &mut Window, win: WinId, module: &ModuleId, why: String, batch: &mut Batch) {
    let Some(peer) = accepting_peer(w, module) else { return };
    with_peer(w, peer, |w, p| {
        let Some(id) = p.entry_of(Role::Destination, module, &[Stage::Accepted]) else {
            return;
        };
        // An accepted offer with no `move` yet is dropped as a source's abort drops it; a received one (none can be
        // here) would be closed by `refuse_received`.
        p.moves.refuse_received(id);
        p.moves.source_aborted(id);
        if let Some(entry) = p.entries.remove(&id) {
            workspace::decided(
                p,
                id,
                &entry,
                crate::window_link::workspace_move::Verdict::Refused(why.clone()),
            );
            // The other window keeps the chat: the review events held for it go to that window.
            chat::release_review(win, &entry, false, batch);
        }
        w.say(format!("[move] {module}: its place could not be made: {why}"));
        send(w, p, Msg::MoveRefused { mv: id.0, why });
    });
}

/// `move` arrived: its descriptors held unread, and `move_done` queued with its confirmation kept; a chat's only once
/// this window's presence holds the session's running turn, so the turn is never unheld by both windows.
fn received(
    w: &mut Window,
    p: &mut PeerMoves,
    win: WinId,
    id: MoveId,
    record: serde_json::Value,
    fds: Descriptors,
    batch: &mut Batch,
) {
    if p.entries
        .get(&id)
        .is_some_and(|e| matches!(e.stage, Stage::Holding | Stage::Received))
    {
        // A second `move` for a move already received: the first is still the one being committed, so neither the
        // arrival nor the source's copy may be given up for it. Its descriptors are closed unread.
        let _ = p.moves.received(id, fds.take());
        w.warn(format!("[move] move {}: a second move for it was ignored", id.0));
        return;
    }
    if let Err(err) = p.moves.received(id, fds.take()) {
        send(
            w,
            p,
            Msg::MoveAbort {
                mv: id.0,
                why: err.to_string(),
            },
        );
        drop_arrival(w, p, win, id, batch);
        return;
    }
    let Some(entry) = p.entries.get_mut(&id) else {
        return;
    };
    entry.record = record;
    entry.fds = Some(fds);
    if entry.kind == MoveKind::Chat {
        if let Some(session) = entry.session.clone() {
            entry.stage = Stage::Holding;
            entry.deadline = Some(w.now() + MOVE_DEADLINE);
            entry.presence_asked = true;
            batch.push_effect(HostOp::Chat(ChatOp::HoldPresence { win, session, mv: id }));
            rearm(w, p);
            return;
        }
    }
    send_done(w, p, win, id, batch);
}

/// `move_done` queued for move `id`, which this window has received and may now hold: its confirmation kept, and
/// the commit made once it is on the socket.
fn send_done(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    let Some(entry) = p.entries.get_mut(&id) else {
        return;
    };
    entry.stage = Stage::Received;
    entry.deadline = None;
    match p.port.send_confirmed(Msg::MoveDone { mv: id.0 }) {
        Ok(answer) => {
            p.moves.done_queued(id, answer);
        }
        Err(err) => {
            // `move_done` never reached the link: the source resumes when the link ends.
            p.moves.done_sent(id, Err(io::Error::other(err.clone())));
            w.warn(format!("[move] move_done for move {} could not be sent: {err}", id.0));
            drop_arrival(w, p, win, id, batch);
        }
    }
}

/// The arrival of move `id` is given up: what was prepared for it goes.
fn drop_arrival(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    let Some(entry) = p.entries.remove(&id) else {
        return;
    };
    workspace::decided(
        p,
        id,
        &entry,
        crate::window_link::workspace_move::Verdict::Refused("its arrival was given up".into()),
    );
    w.say(format!("[move] {}: its arrival was given up", entry.module));
    chat::arrival_given_up(win, id, &entry, batch);
    if chat::is_base_tab(&entry) {
        return;
    }
    batch.push_layout(HostOp::DropSurface {
        win,
        module: entry.module,
        why: DropWhy::MoveGivenUp,
    });
}

/// The destination's moves `Moves` settled: each whose `move_done` is on the socket is committed and built from what
/// it carried; each it dropped has its place dropped. A move whose `move_done` is only queued is left as it is.
fn settle(w: &mut Window, p: &mut PeerMoves, cx: &Cx, win: WinId, batch: &mut Batch) {
    p.moves.settle();
    for id in p.moves.confirmed() {
        commit(w, p, cx, win, id, batch);
    }
    let gone: Vec<MoveId> = p
        .entries
        .iter()
        .filter(|(id, e)| e.role == Role::Destination && !p.moves.holds(Role::Destination, **id))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        drop_arrival(w, p, win, id, batch);
    }
}

/// Move `id`'s descriptors handed over, and only then the module built from them and placed by its open rules (a
/// float where it floated) and mapped, the keys left where they are.
fn commit(w: &mut Window, p: &mut PeerMoves, cx: &Cx, win: WinId, id: MoveId, batch: &mut Batch) {
    let Ok(fds) = p.moves.commit(id) else {
        return;
    };
    let Some(entry) = p.entries.remove(&id) else {
        w.warn(format!("[move] BUG: move {} committed with no arrival", id.0));
        return;
    };
    workspace::decided(p, id, &entry, crate::window_link::workspace_move::Verdict::Committed);
    let descriptors = entry.fds.clone().unwrap_or_default();
    descriptors.put(fds);
    let arrival = entry.module.clone();
    chat::release_review(win, &entry, true, batch);
    if entry.kind != MoveKind::LuaPanel && !(entry.kind == MoveKind::PluginPage && entry.record["built"] == false) {
        batch.push_layout(HostOp::AdoptMoved {
            win,
            module: arrival.clone(),
            kind: entry.kind,
            record: entry.record.clone(),
            fds: descriptors,
        });
    }
    chat::presence_after_adopt(win, id, &entry, batch);
    if chat::is_base_tab(&entry) {
        // The tab joins the base view, which is here already: nothing is placed or declared.
        p.arrived.insert(entry.offered.clone(), arrival.clone());
        w.say(format!("[move] {}: its tab arrived in the base view", entry.offered));
        return;
    }
    let with_workspace = workspace::arrives_with_group(p, id);
    let waiting_page = entry.kind == MoveKind::PluginPage && entry.record["built"] != false;
    place_arrival(w, cx, win, &arrival, entry.shape);
    if waiting_page || with_workspace {
        // Hidden arrivals belong here without taking space until their document or workspace is ready.
        if let Ok(mut layout) = w.layout.try_borrow_mut() {
            frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                let _ = crate::layout::hide(&mut layout, &arrival, frame);
            });
        }
    } else if matches!(entry.kind, MoveKind::PluginPage | MoveKind::LuaPanel) {
        if let Ok(mut layout) = w.layout.try_borrow_mut() {
            let _ = layout.show(&arrival);
        }
    }
    if entry.kind == MoveKind::LuaPanel {
        w.lifecycle.returned(&arrival);
        w.changed_by_controller(win, batch);
        focus::modules_changed(w, win, batch);
        p.arrived.insert(entry.offered.clone(), arrival);
        return;
    }
    let unbuilt = entry.kind == MoveKind::PluginPage && entry.record["built"] == false;
    if unbuilt {
        // Nothing is adopted for a page never built, so the holding is taken here: else its own commands, the
        // one way such a page asks to be shown, would be answered by neither window.
        batch.push_effect(HostOp::Plugin(PluginOp::Hold {
            win,
            module: arrival.clone(),
        }));
        if let Ok(mut layout) = w.layout.try_borrow_mut() {
            frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                let _ = crate::layout::hide(&mut layout, &arrival, frame);
            });
        }
    }
    p.replaced.insert(arrival.clone(), w.lifecycle.saved(&arrival));
    // The arrival keeps its own surface kind so its next show invokes the right service.
    let kind = match entry.kind {
        MoveKind::Chat => SurfaceKind::Chat { view: entry.view() },
        MoveKind::PluginPage => SurfaceKind::PluginPage {
            plugin: arrival.as_str().trim_start_matches("lua:").to_owned(),
        },
        MoveKind::LuaPanel => unreachable!(),
        MoveKind::Terminal => SurfaceKind::Terminal {
            cwd: cx.setup.project_dir.clone(),
        },
    };
    w.lifecycle.declare_moved(&SurfaceDecl {
        module: arrival.clone(),
        kind,
        deferred: unbuilt,
    });
    if waiting_page {
        w.lifecycle.wait_page(&arrival);
        // The waiting page is drawn under the shown modules, and is told it is on screen: a page that draws its
        // restored place from the followed document gets that document only while it is on screen, and must have
        // drawn it before it can say it is ready.
        batch.push_effect(HostOp::Plugin(PluginOp::OnScreen {
            win,
            module: arrival.clone(),
            on: true,
        }));
        let timer = w.arm_after(std::time::Duration::from_secs(1));
        p.pages_waiting.insert(
            arrival.clone(),
            WaitingPage {
                timer,
                since: w.now(),
                workspace: w.workspaces.borrow().current().to_owned(),
                show: !with_workspace,
            },
        );
    }
    w.changed_by_controller(win, batch);
    focus::modules_changed(w, win, batch);
    p.arrived.insert(entry.offered.clone(), arrival.clone());
    w.say(format!("[move] {}: arrived as {arrival}", entry.offered));
}

/// The arriving module put in the layout: back in its placeholder when it was here before, else where the open
/// rules put a new instance (a float at the rectangle it had), and held hidden by every other workspace.
fn place_arrival(w: &mut Window, cx: &Cx, win: WinId, arrival: &ModuleId, shape: Shape) {
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[move] BUG: the layout was borrowed when {arrival} arrived"));
        return;
    };
    if l.contains(arrival) && l.is_away(arrival) {
        let _ = l.set_away(arrival, false);
        drop(l);
        if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
            workspaces.set_away_everywhere(arrival, false);
        }
        return;
    }
    let grid = cx.reads.grid_size(win);
    let keys = l.focus().clone();
    let placed = match shape {
        Shape::Float { x, y, w: fw, h: fh } => {
            let rect = FloatRect {
                cx: (x + fw / 2.0) as f32,
                cy: (y + fh / 2.0) as f32,
                w: (fw * grid.w as f64).round() as i32,
                h: (fh * grid.h as f64).round() as i32,
            }
            .clamped();
            l.add_float(arrival, rect, false).map(|()| l.unzoom()).map(|_| ())
        }
        Shape::Tiled => {
            let mut rule = cx.setup.rules.resolve(arrival, OpenPoint::NewInstance, None).0;
            // An arrival is shown, and the keys stay where they are until the other window gives them.
            rule.focus = false;
            rule.scratchpad = false;
            open_by_rule(&mut l, arrival, &rule, Some(&keys), grid).map(|_| ())
        }
    };
    if let Err(err) = placed {
        // The module exists now and is the only copy there is: it goes where nothing can refuse it.
        w.warn(format!(
            "[move] {arrival}: its place was refused ({err}); floated instead"
        ));
        let _ = l.add_float(arrival, FloatRect::centred(grid, DEFAULT_FLOAT_SHARE), false);
    }
    drop(l);
    let placement = declared_placement(&cx.setup.decls, arrival, Placement::BelowEditor);
    let workspaces = w.workspaces.clone();
    match workspaces.try_borrow_mut() {
        Ok(mut workspaces) => workspaces.add_everywhere_at(arrival, placement),
        Err(_) => w.warn(format!(
            "[move] BUG: the workspaces were borrowed when {arrival} was added to them"
        )),
    };
}

/// The host could not build the arriving module: it was committed, the other window has let it go, and nothing of
/// it runs here, which the user is told. Nothing may go on naming it: it leaves the layout and every workspace and
/// its lifecycle entry is set back to the one it replaced, so its number is free again and a focus for the id the
/// other window gave it no longer finds it. A base id, which never leaves the layout, is left as a killed module:
/// its next show starts a fresh shell in the place prepared for it.
pub fn adopt_refused(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, why: String, batch: &mut Batch) {
    let waiting: Vec<_> = w
        .peers
        .values_mut()
        .filter_map(|p| p.pages_waiting.remove(module))
        .collect();
    for waiting in waiting {
        w.cancel_timer(waiting.timer);
    }
    w.warn(format!("[move] {module}: could not be built here: {why}"));
    if *module == ModuleId::agent() {
        // A tab that could not join the base view: the view and its other tabs stay as they are.
        w.refuse(win, format!("the moved chat could not be opened here: {why}"), batch);
        return;
    }
    let replaced = w
        .peers
        .values_mut()
        .filter_map(|p| {
            p.arrived.retain(|_, arrival| arrival != module);
            p.replaced.remove(module)
        })
        .next();
    let reopen = super::close::reopen(module, &cx.setup.decls);
    let layout = w.layout.clone();
    let next = match layout.try_borrow_mut() {
        Ok(mut l) => {
            let killed = frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                crate::layout::kill(&mut l, module, reopen, frame)
            });
            match killed {
                Ok(next) => next,
                // Killed it cannot be (the last module on screen): a numbered one still leaves the tree.
                Err(err) => {
                    w.warn(format!("[move] {module}: its arrival could not be killed ({err})"));
                    if module.number() >= 2 {
                        if let Err(err) = l.remove(module) {
                            w.warn(format!("[move] BUG: {module} could not leave the layout: {err}"));
                        }
                    }
                    None
                }
            }
        }
        Err(_) => {
            w.warn(format!(
                "[move] BUG: the layout was borrowed when {module}'s arrival was undone"
            ));
            None
        }
    };
    let ended = HostEvent::SurfaceEnded {
        win,
        module: module.clone(),
        why: Ended::Killed { reopen },
    };
    w.lifecycle.record_event(win, &ended);
    if module.number() >= 2 {
        if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
            workspaces.remove_everywhere(module);
        }
        w.lifecycle.restore(module, replaced.unwrap_or(SavedEntry::NONE));
        batch.push_layout(HostOp::DropSurface {
            win,
            module: module.clone(),
            why: DropWhy::MoveGivenUp,
        });
    }
    w.changed_with_focus(win, next.map(|next| (next, vec![])), batch);
    focus::modules_changed(w, win, batch);
    w.refuse(win, format!("{module} could not be built here: {why}"), batch);
}

/// The other window gives this window the keys: an arrival it names by the id it gave it, any module here by its
/// own id, the module that last had them, or the module beside the edge it was left by. The window is asked to come
/// forward in every case, with the activation token when one came. A module named is brought on screen first, as a
/// tray chip brings it (shown when hidden, placed when this workspace lacks it), never hidden.
fn focus_intent(
    w: &mut Window,
    p: &PeerMoves,
    cx: &Cx,
    win: WinId,
    target: FocusTarget,
    token: Option<String>,
    batch: &mut Batch,
) {
    super::start_keys::chosen(w, "the other window gave it the keys");
    batch.push_layout(HostOp::Present {
        win,
        activation: token.map(ActivationToken),
    });
    let to = match target {
        FocusTarget::Module { id } => match ModuleId::parse(&id) {
            Ok(named) => {
                let to = p.arrived.get(&named).cloned().unwrap_or(named);
                if to != ModuleId::editor() {
                    if let Err(err) = super::verbs::modules::reveal_and_focus(w, cx, win, &to, batch) {
                        w.warn(format!("[move] focus for {to}: {err}"));
                    }
                    return;
                }
                to
            }
            Err(err) => {
                w.warn(format!("[move] focus for {id:?}: {err}"));
                return;
            }
        },
        FocusTarget::Window => match w.layout.try_borrow() {
            Ok(layout) => layout.focus().clone(),
            Err(_) => return,
        },
        FocusTarget::Edge { side } => {
            let nav = w.layout.try_borrow().ok().map(|layout| {
                frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                    crate::layout::geometry::navigate_from_edge(
                        &layout,
                        crate::window_link::edge::arrival_side(side).into(),
                        frame,
                    )
                })
            });
            match nav {
                Some(crate::layout::Nav::Module(to)) => to,
                _ => return,
            }
        }
    };
    // The editor can be hidden or ended when the other window asks for its keys.
    if to == ModuleId::editor() {
        if let Err(err) = super::verbs::modules::lua_show(w, cx, win, &to, batch) {
            w.warn(format!("[move] focus for {to}: {err}"));
            return;
        }
    }
    if let Err(err) = focus::focus_and_arrive(w, win, &to, batch) {
        w.warn(format!("[move] focus for {to}: {err}"));
    }
}

// Both sides.

/// `window.other`: `peer` raised, and given the keys once it is up.
pub fn raise_other(w: &mut Window, win: WinId, peer: PeerId, batch: &mut Batch) -> bool {
    raise_for(w, win, peer, FocusTarget::Window, batch)
}

/// Raises `peer` and remembers the intent until the compositor answers; false when it is not attached.
pub fn raise_for(w: &mut Window, win: WinId, peer: PeerId, intent: impl Into<Raise>, batch: &mut Batch) -> bool {
    with_peer(w, peer, |_, p| queue_raise(p, win, intent, batch)).is_some()
}

/// Raises `peer` with the activation token of the module window that asked for it, and remembers the focus the raise
/// is for until the compositor answers; false when the peer is not attached.
pub fn forward_raise(w: &mut Window, win: WinId, peer: PeerId, token: Option<String>, batch: &mut Batch) -> bool {
    with_peer(w, peer, |_, p| {
        batch.push_effect(HostOp::ForwardRaise {
            win,
            peer: p.peer,
            token,
        });
        p.raises.push_back(Raise::Focus(FocusTarget::Window));
    })
    .is_some()
}

/// The peer raised, and what the raise is for remembered until the compositor answers.
fn queue_raise(p: &mut PeerMoves, win: WinId, intent: impl Into<Raise>, batch: &mut Batch) {
    batch.push_effect(HostOp::RaisePeer { win, peer: p.peer });
    p.raises.push_back(intent.into());
}

/// Tells every peer the window's current output when it is known.
pub fn say_monitor(w: &mut Window) {
    let peers: Vec<PeerId> = w.peers().map(|(peer, _)| *peer).collect();
    for peer in peers {
        say_monitor_to(w, peer);
    }
}

/// Tells `peer` the window's current output when it is known.
pub fn say_monitor_to(w: &mut Window, peer: PeerId) {
    let Some(id) = &w.monitor else { return };
    let description = w
        .monitors
        .iter()
        .find(|m| m.id == *id)
        .map(|m| m.name.clone())
        .unwrap_or_default();
    let connector = id.0.clone();
    with_peer(w, peer, |w, p| send(w, p, Msg::Monitor { connector, description }));
}

/// The host could not raise a window: the focus it was for is not sent. The refusal names no peer, so the oldest
/// raise of the first peer that waits for one is dropped.
pub fn raise_refused(w: &mut Window) -> bool {
    w.peers
        .values_mut()
        .find(|p| !p.raises.is_empty())
        .is_some_and(|p| p.raises.pop_front().is_some())
}

/// One fact from the other window or the link, with the destination's moves settled before and after it.
pub fn peer_event(w: &mut Window, cx: &Cx, win: WinId, event: PeerEvent, batch: &mut Batch) {
    let peer = event.peer();
    // An ask starts this window's own offer, which needs the peer in its place: it is settled around, not inside.
    if let PeerEvent::Swap { move_a, move_b, .. } = event {
        workspace::swap(w, cx, win, peer, move_a, move_b, batch);
        return;
    }
    if let PeerEvent::MoveAsk {
        module,
        session,
        follow,
        ..
    } = event
    {
        with_peer(w, peer, |w, p| settle(w, p, cx, win, batch));
        asked(w, cx, win, peer, module, session, follow, batch);
        with_peer(w, peer, |w, p| {
            settle(w, p, cx, win, batch);
            rearm(w, p);
        });
        return;
    }
    with_peer(w, peer, |w, p| {
        // A confirmed `move_done` is committed before the fact is read: the other window may act on the module (a
        // focus intent for it) before this window was woken for the confirmation.
        settle(w, p, cx, win, batch);
        match event {
            PeerEvent::MoveOffer {
                id,
                module,
                kind,
                shape,
                size,
                session,
                ..
            } => offered(w, p, win, id, module, kind, shape, size, session, batch),
            PeerEvent::MoveReady { id, .. } => ready(w, p, win, id, batch),
            PeerEvent::MoveRefused { id, why, .. } => {
                if let Some(verdict) = p.moves.refused(id, &why) {
                    verdict_for(w, p, win, id, verdict, batch);
                }
            }
            PeerEvent::Move { id, record, fds, .. } => received(w, p, win, id, record, fds, batch),
            PeerEvent::MoveDone { id, .. } => {
                // A late `move_done` for a move already resumed is out of turn and answers nothing.
                if let Some(verdict) = p.moves.done(id) {
                    verdict_for(w, p, win, id, verdict, batch);
                }
            }
            PeerEvent::MoveAbort { id, why, .. } => match p.moves.peer_abort(id, &why) {
                PeerAbort::Destination(verdict) => verdict_for(w, p, win, id, verdict, batch),
                PeerAbort::Source => drop_arrival(w, p, win, id, batch),
                PeerAbort::Ignored => {}
            },
            PeerEvent::LinkClosed { .. } | PeerEvent::Bye { .. } => {
                let resumed = p.moves.link_closed();
                verdicts(w, p, win, resumed, batch);
                chat::link_ended(w, p);
                p.raises.clear();
                p.held.clear();
                p.monitor = None;
                p.summary = None;
                if let Some(token) = p.summary_timer.take() {
                    w.cancel_timer(token);
                }
                workspace::link_ended(w, p, win, batch);
                super::verbs::modules::peer_gone(w, peer);
                let was_window_2 = *p.name() == WindowName::Second;
                super::close::moves_link_ended(w, cx, win, peer, was_window_2, batch);
                w.chrome_state.dirty.peer += 1;
            }
            PeerEvent::Raised { token, .. } => {
                let msg = match p.raises.pop_front().unwrap_or(Raise::Focus(FocusTarget::Window)) {
                    Raise::Focus(target) => Msg::Focus { target, token },
                    Raise::Workspace(name) => Msg::Workspace { name, token },
                };
                send(w, p, msg);
            }
            PeerEvent::Summary { summary, .. } => {
                p.summary = Some(crate::window_link::summary::bounded(summary));
                w.chrome_state.dirty.peer += 1;
            }
            PeerEvent::Workspace { name, token, .. } => {
                if let Err(text) = super::verbs::workspaces::switch(w, win, &name, cx.setup, batch) {
                    w.refuse(win, text, batch);
                } else {
                    batch.push_layout(HostOp::Present {
                        win,
                        activation: token.map(ActivationToken),
                    });
                }
            }
            PeerEvent::FocusIntent { target, token, .. } => focus_intent(w, p, cx, win, target, token, batch),
            PeerEvent::Held { ids, .. } => p.held = ids,
            PeerEvent::Monitor { connector, .. } => p.monitor = Some(connector),
            PeerEvent::WorkspaceMove {
                group,
                name,
                tree,
                moves,
                swap,
                ..
            } => {
                workspace::receive(w, p, cx, win, group, name, tree, moves, swap, batch);
            }
            // A module window's asks and focus reports are the controller's, decided before this runs.
            PeerEvent::Woke { .. }
            | PeerEvent::MoveAsk { .. }
            | PeerEvent::Swap { .. }
            | PeerEvent::HubAsk { .. }
            | PeerEvent::WindowFocused { .. } => {}
        }
        settle(w, p, cx, win, batch);
        rearm(w, p);
    });
}

/// `peer` goes away: its moves end as the link's end ends them, and nothing is left of its link here; the window's
/// other peers stay as they are.
pub fn detach(w: &mut Window, cx: &Cx, win: WinId, peer: PeerId, batch: &mut Batch) {
    if !w.peers.contains_key(&peer) {
        return;
    }
    peer_event(w, cx, win, PeerEvent::LinkClosed { peer }, batch);
    let waiting: Vec<_> = w
        .peer(peer)
        .map(|p| p.pages_waiting.keys().cloned().collect())
        .unwrap_or_default();
    for module in waiting {
        show_waiting(w, win, &module, batch);
    }
    if let Some(p) = w.peers.remove(&peer) {
        if let Some((token, _)) = p.deadline {
            w.cancel_timer(token);
        }
        if let Some(token) = p.summary_timer {
            w.cancel_timer(token);
        }
    }
    w.chrome_state.dirty.peer += 1;
}

/// The deadline's timer, if `token` is it: the moves' deadlines judged now.
pub fn deadline_fired(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    let Some(peer) = w
        .peers()
        .find(|(_, p)| p.deadline.map(|(t, _)| t) == Some(token))
        .map(|(peer, _)| *peer)
    else {
        return false;
    };
    with_peer(w, peer, |w, p| {
        p.deadline = None;
        let now = w.now();
        chat::holding_expired(w, p, win, now, batch);
        let due = p.moves.tick(now);
        for (id, verdict) in &due {
            if let SourceVerdict::Resume(why) = verdict {
                // Only a move not yet sent expires: the other window may have prepared a place for it, which it drops
                // only when told.
                send(
                    w,
                    p,
                    Msg::MoveAbort {
                        mv: id.0,
                        why: why.clone(),
                    },
                );
            }
        }
        verdicts(w, p, win, due, batch);
        rearm(w, p);
    });
    true
}

/// The deadline's timer armed for the earliest deadline an open move has, or cancelled when none has.
fn rearm(w: &mut Window, p: &mut PeerMoves) {
    let earliest = p.entries.values().filter_map(|e| e.deadline).min();
    if p.deadline.map(|(_, at)| at) == earliest {
        return;
    }
    if let Some((token, _)) = p.deadline.take() {
        w.cancel_timer(token);
    }
    if let Some(at) = earliest {
        let delay = at.saturating_duration_since(w.now());
        let token = w.arm_after(delay);
        p.deadline = Some((token, at));
    }
}

/// The other window's number is fixed by this end's role in the pair.
pub fn other_name(w: &Window) -> &'static str {
    match side_of(w.role()) {
        Side::One => "2",
        Side::Two => "1",
    }
}

fn current_summary(w: &Window, win: WinId, reads: &dyn HostReads) -> Summary {
    let counts = reads.chat_summary(win);
    let background = reads
        .chat_views()
        .into_iter()
        .chain(std::iter::once(crate::tabs::ViewId::BASE))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|view| reads.chat_kill_counts(view).background)
        .sum::<usize>();
    Summary {
        waiting: counts.waiting,
        unseen: counts.unseen,
        running: counts.running,
        queued: counts.queued,
        background: u32::try_from(background).unwrap_or(u32::MAX),
        // Only the base view's counts carry the window's drafts.
        drafts: u32::try_from(reads.chat_kill_counts(crate::tabs::ViewId::BASE).drafts).unwrap_or(u32::MAX),
        workspaces: w.workspaces.borrow().order().to_vec(),
    }
}

/// A newly attached peer needs current counts before any later chat event arrives.
pub fn summary_at_attach(w: &mut Window, win: WinId, peer: PeerId, reads: &dyn HostReads) {
    let summary = current_summary(w, win, reads);
    with_peer(w, peer, |w, p| {
        send(w, p, Msg::Summary(summary.clone()));
        p.debounce.sent_now(summary.clone());
        p.own_summary = summary;
        p.summary_inputs = (w.chrome_state.dirty.attention, w.chrome_state.dirty.workspaces);
    });
}

/// A tab operation changes host counts while its batch applies, so its read waits for the queue to drain.
pub fn tab_verb(w: &mut Window) {
    for p in w.peers.values_mut() {
        p.after_batch = true;
    }
}

pub fn after_batch(w: &mut Window, win: WinId, reads: &dyn HostReads) {
    let mut waited = false;
    for p in w.peers.values_mut().filter(|p| p.after_batch) {
        p.after_batch = false;
        p.counts_dirty = true;
        waited = true;
    }
    if waited {
        observe_summary(w, win, reads);
    }
}

pub fn counts_changed(w: &mut Window) {
    for p in w.peers.values_mut() {
        p.counts_dirty = true;
    }
}

/// Changes are coalesced after the latest count or workspace change, with one timer per window.
pub fn observe_summary(w: &mut Window, win: WinId, reads: &dyn HostReads) {
    let inputs = (w.chrome_state.dirty.attention, w.chrome_state.dirty.workspaces);
    let due: Vec<PeerId> = w
        .peers()
        .filter(|(_, p)| !(p.after_batch || (p.summary_inputs == inputs && !p.counts_dirty)))
        .map(|(peer, _)| *peer)
        .collect();
    if due.is_empty() {
        return;
    }
    let summary = current_summary(w, win, reads);
    for peer in due {
        with_peer(w, peer, |w, p| {
            p.summary_inputs = inputs;
            p.counts_dirty = false;
            if p.own_summary == summary {
                return;
            }
            p.own_summary = summary.clone();
            let now = w.now();
            p.debounce.observe(now, summary.clone());
            if let Some(token) = p.summary_timer.take() {
                w.cancel_timer(token);
            }
            let after = p
                .debounce
                .due_at()
                .map_or(crate::window_link::summary::SummaryDebounce::DELAY, |at| {
                    at.saturating_duration_since(now)
                });
            p.summary_timer = Some(w.arm_after(after));
        });
    }
}

pub fn summary_fired(w: &mut Window, token: TimerToken) -> bool {
    let Some(peer) = w
        .peers()
        .find(|(_, p)| p.summary_timer == Some(token))
        .map(|(peer, _)| *peer)
    else {
        return false;
    };
    with_peer(w, peer, |w, p| {
        p.summary_timer = None;
        let now = w.now();
        if let Some(summary) = p.debounce.poll(now) {
            send(w, p, Msg::Summary(summary));
        } else if let Some(at) = p.debounce.due_at() {
            // Fired before the debounce's own instant: armed again for it, or the summary would wait for the next
            // count change.
            p.summary_timer = Some(w.arm_after(at.saturating_duration_since(now)));
        }
    });
    true
}

mod page;
pub use page::{page_freeze_refused, page_ready, page_timer, show_waiting};
mod chat;
pub mod workspace;
pub use chat::{ask_session, presence_answered, turn_ended};

#[cfg(test)]
mod tests;
